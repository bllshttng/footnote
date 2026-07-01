//! Dead-row garbage collection decision (x-b1aa).
//!
//! Finished agent-view rows accumulate "like browser tabs": the daemon retires a
//! worker *process* on idle and reconcile flips its status to `exited`, but the
//! *row* lingers until someone `rm`s it. `config.post_merge.self_reap` only fires
//! via the `/pr merged` ritual and reaps by tearing the session down -- unusable
//! for a bg session a human is attached to. This module is the pure decision
//! function both the automatic daemon GC sweep and the manual `fno agents reap`
//! verb call (Locked Decision #2: one decision, two triggers). All I/O -- the
//! liveness re-check, the worktree-cleanliness probe, and the clock -- is done by
//! the caller and passed in, so the policy is unit-testable in isolation.

use crate::AgentStatus;

/// What the GC sweep should do with one registry row this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcAction {
    /// Remove the row now: terminal/dead, strictly past the grace window, and
    /// (for a worktree-owning row) the worktree is clean.
    Reap,
    /// First tick we observe this row dead: stamp `exited_at` to start the grace
    /// clock. The row stays visible for the whole grace window after this.
    StampExit,
    /// Leave the row untouched: still live, still coming up (mid-spawn), inside
    /// the grace window, worktree dirty, or the cleanliness probe failed.
    Keep,
}

/// The probed facts about one registry row the [`gc_action`] policy needs.
#[derive(Debug, Clone, Copy)]
pub struct GcRow {
    /// Registry status (denormalized projection of `state.status`).
    pub status: AgentStatus,
    /// Liveness RE-CHECKED at decision time (AC1-FR): a reachable worker socket
    /// OR a `pid` whose start time still matches what we recorded. A live row is
    /// never touched, so a worker that re-registered during the grace window is
    /// never swept on a stale `exited`.
    pub is_live: bool,
    /// A recorded `pid` is present but is confirmed NOT ours (ESRCH or a recycled
    /// pid whose start time no longer matches): the process is gone even if the
    /// status has not yet been flipped to `Exited`. Lets GC reap a dead row the
    /// reconcile sweep has not visited yet.
    pub pid_confirmed_dead: bool,
    /// A one-shot `ask` row (empty short_id + no pid): it owns no worktree, so the
    /// dirty-worktree guard does not apply -- it is reaped on terminal + grace
    /// alone.
    pub is_ask: bool,
    /// `exited_at` parsed to epoch seconds; `None` when the row is not yet
    /// stamped (never observed dead before).
    pub exited_at: Option<i64>,
    /// Worktree cleanliness for a worktree-owning row: `Some(true)` clean,
    /// `Some(false)` dirty (uncommitted changes -> keep), `None` the probe could
    /// not determine it (fail closed -> keep). Ignored for `is_ask` rows.
    pub worktree_clean: Option<bool>,
}

/// Decide the GC action for one row. Pure: no clock, no I/O.
///
/// The reap condition is all three of: (1) terminal status OR pid confirmed dead
/// (with liveness re-checked, never trusting a stale `exited`), (2) strictly past
/// `grace_secs` since `exited_at`, (3) the worktree is clean (or the row owns
/// none). A row seen dead for the first time is `StampExit`ed rather than reaped,
/// so a just-finished row stays visible for the whole grace window.
pub fn gc_action(row: &GcRow, now: i64, grace_secs: i64) -> GcAction {
    // (AC1-FR) A live worker -- re-checked -- is never touched. The caller clears
    // any stale `exited_at` on such a row separately.
    if row.is_live {
        return GcAction::Keep;
    }
    // Reap condition #1: terminal status OR a confirmed-dead pid. A non-terminal
    // row with no confirmed-dead pid (e.g. `Spawning` with no pid recorded yet)
    // is NOT eligible -- never reap something still coming up.
    let terminal_or_dead = matches!(row.status, AgentStatus::Exited | AgentStatus::PermanentDead)
        || row.pid_confirmed_dead;
    if !terminal_or_dead {
        return GcAction::Keep;
    }
    match row.exited_at {
        // First observation of a dead row: start the grace clock, do not reap yet.
        None => GcAction::StampExit,
        Some(exited) => {
            // Boundary: keep until STRICTLY past the grace window. A row that
            // exited exactly `grace_secs` ago is still kept.
            if now.saturating_sub(exited) <= grace_secs {
                return GcAction::Keep;
            }
            if row.is_ask {
                // No worktree to protect.
                return GcAction::Reap;
            }
            match row.worktree_clean {
                Some(true) => GcAction::Reap,
                // Dirty worktree kept (AC1-EDGE); probe failure fails closed.
                Some(false) | None => GcAction::Keep,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRACE: i64 = 3600; // 1h
    const NOW: i64 = 1_000_000;

    /// A dead, terminal, clean, past-grace worktree row: the AC1-HP base case.
    fn reapable() -> GcRow {
        GcRow {
            status: AgentStatus::Exited,
            is_live: false,
            pid_confirmed_dead: false,
            is_ask: false,
            exited_at: Some(NOW - GRACE - 1),
            worktree_clean: Some(true),
        }
    }

    #[test]
    fn ac1_hp_exited_past_grace_clean_is_reaped() {
        assert_eq!(gc_action(&reapable(), NOW, GRACE), GcAction::Reap);
    }

    #[test]
    fn first_dead_observation_stamps_not_reaps() {
        let row = GcRow {
            exited_at: None,
            ..reapable()
        };
        assert_eq!(gc_action(&row, NOW, GRACE), GcAction::StampExit);
    }

    #[test]
    fn ac1_fr_live_row_is_kept_even_if_stale_exited() {
        // Re-registered worker: status still says exited but liveness re-check
        // reports it live. Never swept.
        let row = GcRow {
            is_live: true,
            exited_at: Some(NOW - GRACE - 999),
            ..reapable()
        };
        assert_eq!(gc_action(&row, NOW, GRACE), GcAction::Keep);
    }

    #[test]
    fn ac1_edge_dirty_worktree_is_kept() {
        let row = GcRow {
            worktree_clean: Some(false),
            ..reapable()
        };
        assert_eq!(gc_action(&row, NOW, GRACE), GcAction::Keep);
    }

    #[test]
    fn probe_failure_fails_closed_kept() {
        let row = GcRow {
            worktree_clean: None,
            ..reapable()
        };
        assert_eq!(gc_action(&row, NOW, GRACE), GcAction::Keep);
    }

    #[test]
    fn within_grace_is_kept() {
        let row = GcRow {
            exited_at: Some(NOW - GRACE + 10),
            ..reapable()
        };
        assert_eq!(gc_action(&row, NOW, GRACE), GcAction::Keep);
    }

    #[test]
    fn exactly_at_grace_boundary_is_kept() {
        // Boundary invariant: kept until STRICTLY past grace.
        let row = GcRow {
            exited_at: Some(NOW - GRACE),
            ..reapable()
        };
        assert_eq!(gc_action(&row, NOW, GRACE), GcAction::Keep);
    }

    #[test]
    fn one_second_past_grace_is_reaped() {
        let row = GcRow {
            exited_at: Some(NOW - GRACE - 1),
            ..reapable()
        };
        assert_eq!(gc_action(&row, NOW, GRACE), GcAction::Reap);
    }

    #[test]
    fn ask_row_ignores_worktree_probe() {
        // An ask row owns no worktree: a dirty/unknown cwd (the user's repo) must
        // not pin it forever.
        let row = GcRow {
            is_ask: true,
            worktree_clean: Some(false),
            ..reapable()
        };
        assert_eq!(gc_action(&row, NOW, GRACE), GcAction::Reap);
    }

    #[test]
    fn non_terminal_pid_none_row_is_kept() {
        // Mid-spawn: Spawning, no pid yet, not live. Must not be reaped/stamped.
        let row = GcRow {
            status: AgentStatus::Spawning,
            pid_confirmed_dead: false,
            exited_at: None,
            ..reapable()
        };
        assert_eq!(gc_action(&row, NOW, GRACE), GcAction::Keep);
    }

    #[test]
    fn pid_confirmed_dead_non_exited_status_is_eligible() {
        // Process gone before reconcile flipped the status: still eligible.
        let row = GcRow {
            status: AgentStatus::Live,
            is_live: false,
            pid_confirmed_dead: true,
            exited_at: None,
            ..reapable()
        };
        assert_eq!(gc_action(&row, NOW, GRACE), GcAction::StampExit);
    }

    #[test]
    fn permanent_dead_is_terminal() {
        let row = GcRow {
            status: AgentStatus::PermanentDead,
            ..reapable()
        };
        assert_eq!(gc_action(&row, NOW, GRACE), GcAction::Reap);
    }
}
