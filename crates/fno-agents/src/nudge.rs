//! P2 loop-boundary announcement nudge.
//!
//! When the loop-check verb returns a `block` decision (the loop-yield
//! boundary), it enriches the decision `message` with the session's unread
//! fleet announcement, so an autonomous loop surfaces it at its next safe
//! boundary (US3/US4). The `message` reaches the continuing model via the
//! stop hook's exit-2 stderr channel (the documented Stop-hook block
//! protocol), so no new vehicle is needed - see AC3-VERIFY.
//!
//! Fleet announcements read NATIVELY from `announce.rs`: they have their own
//! bus line and cursor, so the boundary nudge is one in-process read.
//! (The Python `fno agents nudge-peek` spawn is gone: it
//! shared this boundary - one Python CLI start per block fire, measured at
//! 1,784ms in a traced target fire - instead of porting it.)
//!
//! Fail-open by contract: a missing bus leaves the base message untouched.

use std::path::Path;

/// Append a fleet-announcement nudge to a block-decision message, or return
/// it unchanged.
pub fn append_inbox_nudge(base: &str, cwd: &Path, session_id: &str) -> String {
    // Test/operator escape hatch: the loop-check test suite sets this so its
    // in-process `decide()` calls never touch the bus. Never set in production.
    if std::env::var_os("FNO_NUDGE_DISABLED").is_some() {
        return base.to_string();
    }
    let _ = cwd;
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
    out
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
