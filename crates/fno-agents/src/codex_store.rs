//! The codex transcript-store helpers : where codex lives on disk
//! and what names a rollout file. Moved out of client_verbs.rs, which is
//! shrink-only; the re-export there keeps every existing caller.

/// The codex home the way codex itself resolves it: `$CODEX_HOME` when set,
/// else `~/.codex`. Both codex store readers must resolve the same home, or
/// one of them reads a store the worker never writes.
pub(crate) fn codex_home() -> Option<std::path::PathBuf> {
    if let Ok(h) = std::env::var("CODEX_HOME") {
        if !h.is_empty() {
            return Some(std::path::PathBuf::from(h));
        }
    }
    std::env::var("HOME")
        .ok()
        .map(|h| std::path::PathBuf::from(h).join(".codex"))
}

/// THE codex rollout filename predicate, spelled once: `HarnessStoreIndex`
/// (existence for the death-corroboration side) and rung 4 (freshness for
/// the liveness side) must agree on what a rollout file is, or a store-layout
/// change fixed in one walker silently strands the other. (name, session id)
pub(crate) fn codex_rollout_matches(name: &str, session_id: &str) -> bool {
    name.starts_with("rollout-") && name.contains(session_id)
}

/// Find one rollout for a session without reading or indexing unrelated files.
pub(crate) fn codex_rollout_path(
    root: Option<&std::path::Path>,
    session_id: &str,
) -> Option<std::path::PathBuf> {
    let root = root
        .map(std::borrow::Cow::Borrowed)
        .or_else(|| codex_home().map(|home| std::borrow::Cow::Owned(home.join("sessions"))))?;
    crate::daemon::index_tree(root.as_ref(), 0)
        .ok()?
        .into_iter()
        .find_map(|(name, path)| codex_rollout_matches(&name, session_id).then_some(path))
}

/// One walk of the codex store, as `(filename, mtime secs)` for every rollout
/// file - the sweep-shaped input to rung 4, built ONCE per sweep by
/// `live_liveness_prober` the same way the claude slug dirs are.
/// `None` root resolves `$HOME/.codex/sessions`. An unreadable store answers `None`
/// (fail closed: the rung goes silent, `Unknown`, which keeps).
pub(crate) fn codex_rollout_index(root: Option<&std::path::Path>) -> Option<Vec<(String, u64)>> {
    let root = root
        .map(std::borrow::Cow::Borrowed)
        .or_else(|| codex_home().map(|h| std::borrow::Cow::Owned(h.join("sessions"))))?;
    crate::daemon::index_tree(root.as_ref(), 0)
        .ok()
        .map(|files| {
            files
                .into_iter()
                .filter(|(name, _)| name.starts_with("rollout-"))
                .filter_map(|(name, path)| {
                    let secs = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())?
                        .as_secs();
                    Some((name, secs))
                })
                .collect()
        })
}

/// Rung 4's freshness read against a prebuilt [`codex_rollout_index`]: any
/// rollout for `session_id` written within the window proves the worker is
/// advancing. Fail closed: an absent or unreadable store built no index, and
/// a miss inside one is not-fresh - both read `Unknown`, which keeps.
pub(crate) fn codex_rollout_fresh(index: &[(String, u64)], session_id: &str, now: u64) -> bool {
    index.iter().any(|(name, mtime)| {
        codex_rollout_matches(name, session_id)
            && now.saturating_sub(*mtime) <= crate::client_verbs::CODEX_ROLLOUT_FRESH_SECS
    })
}

/// One codex rollout file with its session id: the session listing
/// [`crate::provenance::CodexSource`] folds.
pub(crate) struct CodexSessionFile {
    pub(crate) path: std::path::PathBuf,
    pub(crate) session_id: String,
    pub(crate) mtime_secs: u64,
    pub(crate) size: u64,
}

/// Every rollout file in the store as a session listing, newest first.
/// `None` root resolves `$CODEX_HOME/sessions` exactly as
/// [`codex_rollout_index`] does; an unreadable store answers empty (the intel
/// fold then reports codex as skipped, never as a guess).
pub(crate) fn codex_sessions(root: Option<&std::path::Path>, days: u64) -> Vec<CodexSessionFile> {
    let resolved = root
        .map(std::borrow::Cow::Borrowed)
        .or_else(|| codex_home().map(|h| std::borrow::Cow::Owned(h.join("sessions"))));
    let Some(root) = resolved else {
        return Vec::new();
    };
    let Ok(files) = crate::daemon::index_tree(root.as_ref(), 0) else {
        return Vec::new();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut out: Vec<CodexSessionFile> = files
        .into_iter()
        .filter(|(name, _)| name.starts_with("rollout-"))
        .filter_map(|(name, path)| {
            let meta = std::fs::metadata(&path).ok()?;
            let mtime_secs = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())?
                .as_secs();
            if days > 0 && now.saturating_sub(mtime_secs) > days * 86_400 {
                return None;
            }
            let session_id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(crate::provenance::rollout_session_id)
                .unwrap_or_else(|| name.clone());
            Some(CodexSessionFile {
                path,
                session_id,
                mtime_secs,
                size: meta.len(),
            })
        })
        .collect();
    out.sort_by(|a, b| b.mtime_secs.cmp(&a.mtime_secs));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_rollout_path_finds_only_matching_session() {
        let root = std::env::temp_dir().join(format!(
            "fno-codex-rollout-path-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let wanted = root.join("rollout-2026-09-24-session-123.jsonl");
        std::fs::write(&wanted, "{}").unwrap();
        std::fs::write(root.join("other-session.jsonl"), "{}").unwrap();
        assert_eq!(codex_rollout_path(Some(&root), "session-123"), Some(wanted));
        assert_eq!(codex_rollout_path(Some(&root), "absent"), None);
        std::fs::remove_dir_all(root).unwrap();
    }
}
