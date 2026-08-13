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
    /// Does this row own a REMOVABLE worktree? Only then does the cleanliness
    /// guard mean anything.
    ///
    /// This started life as `is_ask`, exempting one-shot `ask` rows because they
    /// own no worktree. That was one instance of the real predicate, and the
    /// narrow version pinned 17 of 21 reapable rows on this machine: their `cwd`
    /// is the CANONICAL CHECKOUT, which the row does not own and which is never
    /// clean (a single untracked editor-settings file was enough). The guard
    /// protected nothing there and blocked everything.
    ///
    /// False for a one-shot ask, for a row whose cwd is the canonical checkout,
    /// and for a cwd that is not a linked worktree at all.
    pub owns_worktree: bool,
    /// `exited_at` parsed to epoch seconds; `None` when the row is not yet
    /// stamped (never observed dead before).
    ///
    /// READ THE NAME SCEPTICALLY. This is not when the process exited. It is
    /// when a GC sweep FIRST OBSERVED the row as non-live, written by the only
    /// production writer (`gc_sweep`), which computes one timestamp per pass and
    /// applies it to every newly-observed row. Rows across unrelated tenants and
    /// projects therefore share a stamp to the second; processes do not exit in
    /// synchronised batches, and that batching is the proof the field measures a
    /// sweep tick rather than an exit.
    ///
    /// So a stamp is evidence that a sweep once failed to reach a worker, not
    /// that the worker died. A claude bg thread that finished a turn is idle and
    /// resumable, and `fno agents ask` correctly calls it "live but not currently
    /// routable" while the registry says exited and stamps this clock. Never reap
    /// on this field alone; see `transcript_fresh`.
    pub exited_at: Option<i64>,
    /// Does this row have any way of being alive at all? True when it records a
    /// pid or a short_id; false for a one-shot `ask` row, which carries neither.
    ///
    /// A row with no liveness surface needs no corroboration, because there is no
    /// worker there to protect. This is a POSITIVE fact about the row rather than
    /// an exemption: nothing can be running behind an identity that was never
    /// recorded.
    pub liveness_surface: bool,
    /// The second, INDEPENDENT liveness signal, required before any reap.
    ///
    /// `Some(true)` the worker's transcript was written recently (it is alive, or
    /// at least idle-and-resumable), `Some(false)` positively stale, `None` we
    /// could not tell.
    ///
    /// Transcript mtime is used because this repo has already paid for the
    /// lesson: receipts, manifest snapshots, process argv, and liveness probes
    /// have each lied about a live session, and only the live lockfile and the
    /// transcript stayed truthful.
    ///
    /// Reaping requires `Some(false)` — a POSITIVE reading of staleness. `None`
    /// keeps the row, because an absence of freshness has two explanations and
    /// only one of them is a dead worker.
    pub transcript_fresh: Option<bool>,
    /// Worktree cleanliness for a worktree-owning row: `Some(true)` clean,
    /// `Some(false)` dirty (uncommitted changes -> keep), `None` the probe could
    /// not determine it (fail closed -> keep). Ignored when `owns_worktree` is
    /// false, because then there is nothing for it to protect.
    pub worktree_clean: Option<bool>,
}

/// Decide the GC action for one row. Pure: no clock, no I/O.
///
/// The reap condition is all three of: (1) terminal status OR pid confirmed dead
/// (with liveness re-checked, never trusting a stale `exited`), (2) strictly past
/// `grace_secs` since `exited_at`, (3) the worktree is clean, OR the row owns no
/// worktree for the guard to protect. A row seen dead for the first time is
/// `StampExit`ed rather than reaped, so a just-finished row stays visible for the
/// whole grace window.
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
            // CORROBORATION GATE. `status` and `exited_at` both derive from one
            // sweep's failure to reach a worker, so they are a single signal
            // wearing two hats and cannot confirm each other. Removal needs at
            // least one POSITIVE, independent reading that the worker is gone.
            //
            // Three qualify. A pid whose start time no longer matches is a
            // process that provably ended. A transcript untouched for the whole
            // window is a session that provably stopped writing. A row with no
            // liveness surface at all never had a worker to lose.
            //
            // None of them available means keep. Reaping a live session destroys
            // work in progress and, worse, the only process able to satisfy its
            // own PR's review gate.
            let independently_gone = row.pid_confirmed_dead
                || row.transcript_fresh == Some(false)
                || !row.liveness_surface;
            if !independently_gone {
                return GcAction::Keep;
            }
            if !row.owns_worktree {
                // No worktree to protect, so nothing for cleanliness to say.
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
            owns_worktree: true,
            exited_at: Some(NOW - GRACE - 1),
            liveness_surface: true,
            transcript_fresh: Some(false),
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
            owns_worktree: false,
            worktree_clean: Some(false),
            ..reapable()
        };
        assert_eq!(gc_action(&row, NOW, GRACE), GcAction::Reap);
    }

    // -- The corroboration gate ---------------------------------------------
    //
    // THE SPECIMEN. A worker named `testspawn` (short_id 626ef4a2) answered its
    // prompt and went idle. Four surfaces gave three verdicts:
    //
    //   registry.json     status "exited", exited_at 2026-08-13T18:52:57Z
    //   fno agents ask    "live but not currently routable"
    //   fno agents peek   by short_id: not found; by name: renders the transcript
    //   agent view        Done
    //
    // Done is a TURN state, not a process state. A claude bg thread that finished
    // a turn is idle and resumable, and its transcript is what says so.

    #[test]
    fn a_stamped_row_whose_transcript_is_fresh_is_never_reaped() {
        // The specimen. `status` and `exited_at` both say gone; the transcript
        // says otherwise, and the transcript wins.
        let row = GcRow {
            transcript_fresh: Some(true),
            pid_confirmed_dead: false,
            ..reapable()
        };
        assert_eq!(gc_action(&row, NOW, GRACE), GcAction::Keep);
    }

    #[test]
    fn an_unreadable_transcript_keeps_the_row() {
        // No positive reading of death -> no removal. An absence of freshness has
        // two explanations and only one is a dead worker.
        let row = GcRow {
            transcript_fresh: None,
            pid_confirmed_dead: false,
            ..reapable()
        };
        assert_eq!(gc_action(&row, NOW, GRACE), GcAction::Keep);
    }

    #[test]
    fn status_and_exited_at_alone_never_authorise_a_reap() {
        // They are ONE signal wearing two hats: `gc_sweep` writes the stamp when a
        // sweep fails to reach a worker, so it cannot corroborate the status that
        // produced it. Batched stamps shared to the second across unrelated
        // tenants are the proof.
        let row = GcRow {
            status: AgentStatus::Exited,
            is_live: false,
            pid_confirmed_dead: false,
            owns_worktree: false,
            exited_at: Some(NOW - GRACE - 1),
            liveness_surface: true,
            transcript_fresh: None,
            worktree_clean: Some(true),
        };
        assert_eq!(gc_action(&row, NOW, GRACE), GcAction::Keep);
    }

    #[test]
    fn a_confirmed_dead_pid_is_independent_evidence_on_its_own() {
        // A pid whose start time no longer matches is a process that provably
        // ended. That does not come from the sweep, so it corroborates.
        let row = GcRow {
            pid_confirmed_dead: true,
            transcript_fresh: None,
            ..reapable()
        };
        assert_eq!(gc_action(&row, NOW, GRACE), GcAction::Reap);
    }

    #[test]
    fn a_row_with_no_liveness_surface_needs_no_corroboration() {
        // A one-shot ask records neither pid nor short_id. Nothing can be running
        // behind an identity that was never recorded, so there is nobody to
        // protect. This is a positive fact, not an exemption.
        let row = GcRow {
            liveness_surface: false,
            pid_confirmed_dead: false,
            transcript_fresh: None,
            owns_worktree: false,
            ..reapable()
        };
        assert_eq!(gc_action(&row, NOW, GRACE), GcAction::Reap);
    }

    #[test]
    fn corroboration_never_overrides_the_earlier_guards() {
        // A positive death signal permits a reap; it must not skip liveness, the
        // grace window, or terminal status.
        let live = GcRow {
            is_live: true,
            pid_confirmed_dead: true,
            transcript_fresh: Some(false),
            ..reapable()
        };
        assert_eq!(gc_action(&live, NOW, GRACE), GcAction::Keep);

        let in_grace = GcRow {
            exited_at: Some(NOW - GRACE + 10),
            transcript_fresh: Some(false),
            ..reapable()
        };
        assert_eq!(gc_action(&in_grace, NOW, GRACE), GcAction::Keep);
    }

    #[test]
    fn row_in_the_canonical_checkout_is_not_pinned_by_its_dirt() {
        // THE MEASURED CASE. 17 of 21 past-grace rows on this machine had `cwd`
        // = the canonical checkout, kept by a single untracked editor-settings
        // file. The row does not own that checkout and cannot remove it, so its
        // dirt says nothing about whether the ROW may go.
        let row = GcRow {
            owns_worktree: false,
            worktree_clean: Some(false),
            ..reapable()
        };
        assert_eq!(gc_action(&row, NOW, GRACE), GcAction::Reap);
    }

    #[test]
    fn a_row_that_does_own_a_dirty_worktree_is_still_kept() {
        // The generalisation must not swallow the case the guard exists for.
        // Two of the 21 were exactly this: a real linked worktree with real
        // uncommitted edits. Those stay.
        let row = GcRow {
            owns_worktree: true,
            worktree_clean: Some(false),
            ..reapable()
        };
        assert_eq!(gc_action(&row, NOW, GRACE), GcAction::Keep);
    }

    #[test]
    fn owning_no_worktree_never_shortcuts_liveness_or_grace() {
        // `owns_worktree: false` skips ONE check. It must not become a fast path
        // that reaps a live row or one still inside its grace window.
        let live = GcRow {
            owns_worktree: false,
            is_live: true,
            ..reapable()
        };
        assert_eq!(gc_action(&live, NOW, GRACE), GcAction::Keep);

        let in_grace = GcRow {
            owns_worktree: false,
            exited_at: Some(NOW - GRACE + 10),
            ..reapable()
        };
        assert_eq!(gc_action(&in_grace, NOW, GRACE), GcAction::Keep);

        let not_terminal = GcRow {
            owns_worktree: false,
            status: AgentStatus::Idle,
            pid_confirmed_dead: false,
            ..reapable()
        };
        assert_eq!(gc_action(&not_terminal, NOW, GRACE), GcAction::Keep);
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
