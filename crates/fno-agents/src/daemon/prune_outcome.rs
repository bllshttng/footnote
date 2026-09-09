//! What actually happened to a prune attempt, so a caller can branch on
//! the outcome instead of recording intent as fact.

/// `Removed` carries the worktree path; `Kept` carries the reason already
/// built by the gate or the removal attempt (the blocked gate, the
/// unanswerable probe, or the failed `git worktree remove`), formatted
/// exactly as `receipt()` prints it below.
pub(crate) enum PruneOutcome {
    Removed(String),
    Kept(String),
}

impl PruneOutcome {
    /// The exact strings `rm_take_worktree_with` printed before this type
    /// existed - callers that only want the receipt text keep reading it
    /// unchanged.
    pub(crate) fn receipt(&self) -> String {
        match self {
            PruneOutcome::Removed(path) => format!("worktree removed: {path}"),
            PruneOutcome::Kept(reason) => format!("worktree kept: {reason}"),
        }
    }
}
