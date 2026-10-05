//! The machine-armed hold mark: a bus-only row whose mail-hold
//! sidecar carries a live conversation-sourced clock is `[HELD]`, never the
//! `[DND]` a lead reads as a hold it armed itself. Read-time projection over
//! the sidecars the conversation arm already writes - no registry field, so
//! the badge follows the hold's real lifecycle (release clears the policy and
//! the clock together) with nothing new to keep consistent.

use super::{rfc3339_like_to_secs, RegistryAgent};
use std::path::{Path, PathBuf};

/// Stamp `held_conversation` onto rows the mail-hold sidecars mark
/// machine-armed. Fail-quiet like `overlay_truth_badges`: an unreadable
/// sidecar degrades to no mark - the row shows `[DND]`, today's label,
/// never a wrong one.
pub(crate) fn overlay_hold_marks(rows: &mut [RegistryAgent], now_secs: u64) {
    overlay_hold_marks_at(rows, &hold_dir(), now_secs);
}

/// The mail-hold dir, mirroring `mail_hold.rs`'s `state_root()`
/// (`FNO_HOME` > `$HOME/.fno`) so the overlay reads the root the arm
/// wrote. A global-config `paths.state_dir` root is not mirrored here;
/// that install degrades to no mark, never a wrong one.
fn hold_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("FNO_HOME") {
        return PathBuf::from(home).join("mail-hold");
    }
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".fno"))
        .unwrap_or_else(|| PathBuf::from(".fno"))
        .join("mail-hold")
}

pub(crate) fn overlay_hold_marks_at(rows: &mut [RegistryAgent], hold_dir: &Path, now_secs: u64) {
    for row in rows.iter_mut() {
        if !row.dnd {
            continue;
        }
        let Some(sid) = row.harness_session_id.as_deref() else {
            continue;
        };
        // mail_hold.rs `identity_key`: ses_ ids verbatim, else lowercased.
        let key = if sid.starts_with("ses_") {
            sid.to_string()
        } else {
            sid.to_lowercase()
        };
        let Ok(text) = std::fs::read_to_string(hold_dir.join(format!("{key}.json"))) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let live = v
            .get("until")
            .and_then(|u| u.as_str())
            .and_then(rfc3339_like_to_secs)
            .is_some_and(|until| until > now_secs);
        if live && v.get("source").and_then(|s| s.as_str()) == Some("conversation") {
            row.held_conversation = true;
        }
    }
}
