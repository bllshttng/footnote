//! How `handle_rm`'s blocking chain runs without stalling the daemon
//! executor (x-4775). Extracted from the parent under the file-budget gate:
//! `off_executor`, the directory-size walk it bounds, and the
//! reclaimed-bytes null-vs-zero rule all moved with the code they touched.

use super::*;

/// Run blocking work from inside an async handler that cannot use
/// `run_blocking`, without starving the executor.
///
/// `handle_rm_with` is the one lifecycle handler that is neither: it awaits
/// real socket I/O (`stop_worker_confirmed`) AND shells out. Its subprocess
/// chain is bounded per call but adds up - a claude removal (15s), a pane kill
/// (60s), the reapable probe plus four `branch_merged` git calls (300s) and
/// `git worktree remove` (60s) - and most of it runs after the registry row is
/// already gone. Run inline, that occupies a runtime worker thread for the
/// whole span and every other verb queues behind it.
///
/// `spawn_blocking` is not available here: the handler borrows injected
/// `&dyn Fn` test seams, which its `'static` bound rejects. `block_in_place`
/// takes borrowed work but needs a multi-thread runtime; the daemon builds one
/// (`bin/daemon.rs`), and unit tests run on `current_thread`, where the call is
/// already alone on its thread and calling `f` directly is correct rather than
/// a fallback.
pub(super) fn off_executor<T>(f: impl FnOnce() -> T) -> T {
    use tokio::runtime::{Handle, RuntimeFlavor};
    match Handle::try_current().map(|handle| handle.runtime_flavor()) {
        Ok(RuntimeFlavor::MultiThread) => tokio::task::block_in_place(f),
        _ => f(),
    }
}

/// Wall-clock budget for [`directory_bytes`]. It is the last unbounded wait
/// left in the `rm` path (x-4775): every subprocess in the chain already
/// carries a timeout, but a recursive `read_dir` walk over a large or
/// slow-storage worktree does not. Checked once per directory entered, not
/// per file, so the cost of checking never dominates the walk itself.
pub(super) const DIRECTORY_BYTES_BUDGET: Duration = Duration::from_secs(10);

// pub(crate): the merge-reaper (merge_reap.rs) also measures a worktree's
// size before taking it, budget-bound the same way rm's walk is.
pub(crate) fn directory_bytes(path: &std::path::Path) -> Option<u64> {
    directory_bytes_within(path, DIRECTORY_BYTES_BUDGET)
}

/// `budget` is a parameter (rather than baking `DIRECTORY_BYTES_BUDGET` in
/// directly) so a test can prove the deadline fires without waiting 10s for
/// real.
pub(super) fn directory_bytes_within(path: &std::path::Path, budget: Duration) -> Option<u64> {
    fn walk(
        path: &std::path::Path,
        total: &mut u64,
        deadline: std::time::Instant,
    ) -> std::io::Result<()> {
        if std::time::Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "directory_bytes budget exceeded",
            ));
        }
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let metadata = std::fs::symlink_metadata(entry.path())?;
            if metadata.is_dir() {
                walk(&entry.path(), total, deadline)?;
            } else {
                *total = total.saturating_add(metadata.len());
            }
        }
        Ok(())
    }
    let deadline = std::time::Instant::now() + budget;
    let mut total = 0;
    walk(path, &mut total, deadline).ok().map(|()| total)
}

/// The `rm` receipt's `reclaimed_bytes`, `Option<u64>` so a removed tree
/// whose size walk hit its budget serializes as `null` rather than `0` -
/// both would otherwise read as "nothing reclaimed" to a caller that cannot
/// tell "measured, zero bytes" from "never measured" apart (x-4775). A tree
/// that was NOT removed keeps reporting `0`, because that zero is true
/// regardless of whether the walk ran. `explicit` is the audit override
/// (`audit_reclaimed_bytes`, tests only); it wins unconditionally when set.
pub(super) fn resolve_reclaimed_bytes(
    explicit: Option<u64>,
    worktree_removed: bool,
    measured: Option<u64>,
) -> Option<u64> {
    match explicit {
        Some(explicit) => Some(explicit),
        None if worktree_removed => measured,
        None => Some(0),
    }
}
