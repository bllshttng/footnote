//! The codex transcript-store helpers (x-70e1): where codex lives on disk
//! and what names a rollout file. Moved out of client_verbs.rs, which is
//! shrink-only; the re-export there keeps every existing caller.

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

/// One walk of the codex store, as `(filename, mtime secs)` for every rollout
/// file - the sweep-shaped input to rung 4, built ONCE per sweep by
/// `live_liveness_prober` the same way the claude socket index is. `None`
/// root resolves `$HOME/.codex/sessions`. An unreadable store answers `None`
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
