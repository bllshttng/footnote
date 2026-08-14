//! Transcript-location + detach-sentinel primitives shared by the live-inject
//! transport (`crates/fno-agents/src/mail_inject.rs`).
//!
//! G1 substrate (epic x-07c1, node x-26df). This file originally also carried
//! the drive primitive itself (`control.sock op:'reply' {short,text,auth}`,
//! confirmed by a new assistant transcript turn) -- superseded, and removed as
//! dead code, by node x-1904: the live transport is the bracketed-paste +
//! wire-level-CR inject in `mail_inject.rs` (`inject_with_submit` /
//! `confirm_content_after`), which confirms by CONTENT rather than by
//! assistant-turn role, and whose `<fno_mail>` envelope is rendered
//! Python-side rather than by this crate's `FnoMail`/`wrap_fno_mail`. Nothing
//! outside this file's own tests called `inject_reply` or `drive_and_confirm`
//! any more; a working-looking second implementation of the inject path is
//! exactly the N-reachable-paths trap the pitfalls corpus warns about, so it
//! is gone rather than kept as an unreachable "just in case".
//!
//! What remains is genuinely shared:
//!   - transcript lookup by full `session_uuid`, globbed across project dirs
//!     (mirrors `cli/src/fno/doctor.py` `_find_transcript_for`), sidestepping
//!     the lossy cwd-encoding;
//!   - the detach-sentinel guard (`[corroborated]`): injecting either sentinel
//!     would tear the session off the daemon, so any inject path refuses text
//!     containing one before writing a byte.
//!
//! Addressing keys on the FULL `session_uuid`; `short` is a wire-derived value
//! (`sessionId.split('-')[0]`) used only at the `control.sock` boundary.

use std::path::{Path, PathBuf};

/// Detach sentinels Claude's client uses to pull a session off the daemon.
/// Injecting either would detach the session, so an inject path refuses any
/// text containing one. `[corroborated]`
pub const DETACH_SENTINELS: [&str; 2] = ["\x1b_cc-daemon-detach\x1b\\", "\x1b_cc-detach-msg;"];

/// Env override for the Claude projects (transcript) base dir (tests). When
/// unset, `$HOME/.claude/projects`.
pub const PROJECTS_DIR_ENV: &str = "FNO_CLAUDE_PROJECTS_DIR";

/// What can go wrong driving a turn.
#[derive(Debug)]
pub enum DriveError {
    /// The text contains a detach sentinel; refused before any write.
    UnsafeText,
    /// An I/O error (socket write, transcript read).
    Io(String),
}

impl std::fmt::Display for DriveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DriveError::UnsafeText => write!(f, "refused: text contains a detach sentinel"),
            DriveError::Io(s) => write!(f, "io error: {s}"),
        }
    }
}

impl std::error::Error for DriveError {}

/// The Claude projects (transcript) base dir.
pub fn claude_projects_dir() -> PathBuf {
    if let Some(v) = std::env::var_os(PROJECTS_DIR_ENV) {
        return PathBuf::from(v);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".claude").join("projects")
}

/// Loose `8-4-4-4-12` hex session-uuid check (the transcript filename shape).
fn is_session_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && [8, 4, 4, 4, 12]
            .iter()
            .zip(&parts)
            .all(|(&n, p)| p.len() == n && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Locate a session's transcript JSONL by its full uuid, across all project dirs.
/// Mirrors `doctor.py`: the filename is the uuid, so we never need the lossy
/// cwd-encoding. Returns `None` for a malformed uuid or when no transcript exists.
pub fn find_transcript(session_uuid: &str) -> Option<PathBuf> {
    if !is_session_uuid(session_uuid) {
        return None;
    }
    let base = claude_projects_dir();
    let entries = std::fs::read_dir(&base).ok()?;
    for entry in entries.flatten() {
        let candidate = entry.path().join(format!("{session_uuid}.jsonl"));
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// True if `text` contains any detach sentinel.
pub fn contains_detach_sentinel(text: &str) -> bool {
    DETACH_SENTINELS.iter().any(|s| text.contains(s))
}

/// Current byte length of `path` (the baseline to read new transcript lines from),
/// or 0 if absent/unreadable.
pub fn transcript_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "fno-drive-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    const UUID: &str = "a1b2c3d4-1111-2222-3333-444455556666";

    #[test]
    fn is_session_uuid_validates_shape() {
        assert!(is_session_uuid(UUID));
        assert!(!is_session_uuid("a1b2c3d4")); // short id, not a uuid
        assert!(!is_session_uuid("not-a-uuid"));
        assert!(!is_session_uuid("g1b2c3d4-1111-2222-3333-444455556666")); // non-hex
    }

    #[test]
    fn find_transcript_by_uuid_across_project_dirs() {
        let base = tmpdir("find");
        std::env::set_var(PROJECTS_DIR_ENV, &base);
        let proj = base.join("-Users-x-code-proj");
        std::fs::create_dir_all(&proj).unwrap();
        let t = proj.join(format!("{UUID}.jsonl"));
        std::fs::write(&t, b"{}\n").unwrap();

        assert_eq!(find_transcript(UUID), Some(t));
        assert_eq!(
            find_transcript("ffffffff-0000-0000-0000-000000000000"),
            None
        );
        assert_eq!(find_transcript("bad-uuid"), None);
        std::env::remove_var(PROJECTS_DIR_ENV);
        std::fs::remove_dir_all(&base).ok();
    }
}
