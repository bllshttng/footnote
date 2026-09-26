//! grok's session store: where a session's accepted turns are recorded, and
//! how one session id resolves to its `chat_history.jsonl`.
//!
//! Layout (grok 1.0.34, bundled sessions guide): each session lives in
//! `$GROK_HOME/sessions/<encoded-cwd>/<session-id>/`, and
//! `chat_history.jsonl` inside it holds the raw chat messages sent to the
//! model, with the typed prompt recorded JSON-escaped. That file is the one
//! local accepted-turn record both the Stop-gate transcript read and the
//! keeper-mail content confirm need.

use crate::pi::SessionLookup;
use std::path::{Path, PathBuf};

/// grok's sessions root: `$GROK_HOME/sessions` when GROK_HOME is set and
/// non-empty, else `$HOME/.grok/sessions`. grok derives its base directory
/// from GROK_HOME with `~/.grok` as the default.
pub fn grok_sessions_root() -> PathBuf {
    let home = std::env::var("GROK_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("{}/.grok", std::env::var("HOME").unwrap_or_default()));
    PathBuf::from(home).join("sessions")
}

/// Resolve one grok session id to its `chat_history.jsonl`.
///
/// grok groups sessions under a URL-encoded working directory and falls back
/// to a slug-plus-hash name when that encoding passes 255 bytes, so the group
/// name is not worth re-deriving. Scan one level deep for
/// `<group>/<session-id>/chat_history.jsonl` instead: the id is a UUID and the
/// scan is encoding-agnostic. The `One`/`None`/`Duplicate`/`Unknown`
/// vocabulary and its discipline are pi's, reused verbatim.
pub fn lookup_session(root: &Path, session_id: &str) -> SessionLookup {
    // An empty id can never name a session, and PathBuf::join("") would turn
    // each group path into its own candidate root, matching a stray file.
    if session_id.is_empty() {
        return SessionLookup::None;
    }
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => {
            return SessionLookup::Unknown {
                dir: root.to_path_buf(),
                reason: error.to_string(),
            }
        }
    };
    let mut files: Vec<PathBuf> = Vec::new();
    for group in entries.filter_map(Result::ok) {
        let candidate = group.path().join(session_id).join("chat_history.jsonl");
        if candidate.is_file() {
            files.push(candidate);
        }
    }
    files.sort();
    match files.len() {
        0 => SessionLookup::None,
        1 => SessionLookup::One {
            file: files.remove(0),
        },
        _ => SessionLookup::Duplicate { files },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_dir(base: &Path, group: &str, session: &str) -> PathBuf {
        let dir = base.join(group).join(session);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn one_hit_resolves_the_store_file() {
        let base = std::env::temp_dir().join(format!("grok-store-one-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let file = store_dir(&base, "%2Frepo", "sid-1").join("chat_history.jsonl");
        std::fs::write(&file, "{}\n").unwrap();
        match lookup_session(&base, "sid-1") {
            SessionLookup::One { file: found } => assert_eq!(found, file),
            other => panic!("want One, got {other:?}"),
        }
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn no_hit_reads_none_and_missing_root_reads_unknown() {
        let base = std::env::temp_dir().join(format!("grok-store-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        assert!(matches!(
            lookup_session(&base, "sid-1"),
            SessionLookup::None
        ));
        // The empty id reads None even with files present: join("") would
        // otherwise match a group-root chat_history.jsonl.
        assert!(matches!(lookup_session(&base, ""), SessionLookup::None));
        let missing = base.join("absent");
        assert!(matches!(
            lookup_session(&missing, "sid-1"),
            SessionLookup::Unknown { .. }
        ));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn two_groups_with_one_id_read_duplicate() {
        let base = std::env::temp_dir().join(format!("grok-store-dup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        for group in ["%2Frepo-a", "%2Frepo-b"] {
            let file = store_dir(&base, group, "sid-1").join("chat_history.jsonl");
            std::fs::write(&file, "{}\n").unwrap();
        }
        match lookup_session(&base, "sid-1") {
            SessionLookup::Duplicate { files } => assert_eq!(files.len(), 2),
            other => panic!("want Duplicate, got {other:?}"),
        }
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn root_env_wins_over_home_default() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let saved = std::env::var_os("GROK_HOME");
        std::env::set_var("GROK_HOME", "/tmp/grok-home-pin");
        assert_eq!(
            grok_sessions_root(),
            PathBuf::from("/tmp/grok-home-pin/sessions")
        );
        std::env::set_var("GROK_HOME", "");
        let home = std::env::var("HOME").unwrap_or_default();
        assert_eq!(
            grok_sessions_root(),
            PathBuf::from(format!("{home}/.grok/sessions"))
        );
        match saved {
            Some(v) => std::env::set_var("GROK_HOME", v),
            None => std::env::remove_var("GROK_HOME"),
        }
    }
}
