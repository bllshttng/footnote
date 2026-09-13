//! P2 loop-boundary inbox nudge (ab-098967b4).
//!
//! When the loop-check verb returns a `block` decision (the loop-yield
//! boundary), it enriches the decision `message` with a one-line nudge for the
//! oldest unread inbox message addressed to this session's project, so an
//! autonomous loop surfaces mail at its next safe boundary (US3/US4). The
//! `message` reaches the continuing model via the stop hook's exit-2 stderr
//! channel (the documented Stop-hook block protocol), so no new vehicle is
//! needed — see AC3-VERIFY.
//!
//! Discovery + the inbox scan + the per-session "surface once" cursor all live
//! in Python (`fno agents nudge-peek`), reused here via a fail-open shell-out:
//! the Python side already owns the bus reader and the project resolver, and
//! keeping the logic there avoids a second, drifting implementation. This file
//! is intentionally OUTSIDE the historical `loop*` control-plane glob so the
//! delta stays minimal — `loopcheck.rs` only calls in.
//!
//! Fleet announcements ride the SAME boundary but read NATIVELY from
//! `announce.rs`: they have their own bus line and cursor, so the addressed-mail
//! shell-out below stays untouched.
//!
//! Fail-open by contract: a missing `fno`, a non-zero exit, or empty output
//! leaves the base message untouched. The nudge fires only on a `block` return
//! (never on allow/terminate), exactly once per message (the Python cursor).

use std::path::Path;
use std::process::Command;

/// Append a one-line inbox nudge to a block-decision message, or return it
/// unchanged. `FNO_AGENTS_RUNTIME=python` pins the child to the Python
/// dispatch so the internal `nudge-peek` verb cannot recurse into this binary.
pub fn append_inbox_nudge(base: &str, cwd: &Path, session_id: &str) -> String {
    // Test/operator escape hatch: the loop-check test suite sets this so its
    // in-process `decide()` calls never spawn the Python helper (latency +
    // filesystem side-effects). Never set in production.
    if std::env::var_os("FNO_NUDGE_DISABLED").is_some() {
        return base.to_string();
    }
    // The loop boundary is a delivery boundary for fleet
    // announcements. Native read (own cursor), fail-open.
    let mut out = String::from(base);
    if let Some(paths) = crate::announce::AnnouncePaths::from_env_opt() {
        if let Ok(Some(announce)) =
            crate::announce::read_render(&paths, session_id, crate::announce::Boundary::Loop)
        {
            out.push_str("\n\n");
            out.push_str(&announce);
        }
    }
    let output = Command::new("fno")
        .args(["agents", "nudge-peek", "--session-id", session_id, "--cwd"])
        .arg(cwd)
        .env("FNO_AGENTS_RUNTIME", "python")
        .output();
    let stdout = match output {
        Ok(o) if o.status.success() => o.stdout,
        _ => return out,
    };
    let nudge = String::from_utf8_lossy(&stdout);
    let nudge = nudge.trim();
    if nudge.is_empty() {
        out
    } else {
        format!("{out}\n\n{nudge}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_returns_base_unchanged() {
        std::env::set_var("FNO_NUDGE_DISABLED", "1");
        let out = append_inbox_nudge("continue working", Path::new("/tmp"), "sess-1");
        assert_eq!(out, "continue working");
    }
}
