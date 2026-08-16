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

/// How many grace windows a row with nothing to corroborate is kept before the
/// absolute-age backstop removes it. Large on purpose: the backstop is the
/// escape hatch for rows a data defect made unjudgeable, not a second normal
/// path out of the registry.
pub const BACKSTOP_GRACE_MULTIPLE: i64 = 168;

/// Absolute floor for the backstop horizon, in seconds (7 days).
///
/// A horizon derived purely by multiplication collapses with its input: a
/// configured `agents.dead_row_grace` of 0 (`FNO_AGENTS_DEAD_ROW_GRACE_SECS=0`
/// or the config scalar, neither clamped) makes the product 0, and then EVERY
/// uncorroborated row reaps on the next tick - the exact inversion the
/// corroboration gate exists to prevent. The floor is what the default grace
/// already multiplies out to (3600 * 168), so a default config sees no change.
pub const BACKSTOP_MIN_HORIZON_SECS: i64 = 3600 * BACKSTOP_GRACE_MULTIPLE;

/// Seconds a row with nothing to corroborate is kept before the absolute-age
/// backstop is willing to remove it. Never below [`BACKSTOP_MIN_HORIZON_SECS`].
pub fn backstop_horizon_secs(grace_secs: i64) -> i64 {
    grace_secs
        .saturating_mul(BACKSTOP_GRACE_MULTIPLE)
        .max(BACKSTOP_MIN_HORIZON_SECS)
}

/// What the GC sweep should do with one registry row this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcAction {
    /// Remove the row now: terminal/dead, strictly past the grace window, and
    /// (for a worktree-owning row) the worktree is clean.
    Reap,
    /// Remove the row on absolute age alone, with NO corroborating signal.
    ///
    /// The corroboration gate below is fail-closed on purpose, and unbounded
    /// growth is its honest cost: a row carrying an identity but neither a pid
    /// nor a transcript offers nothing to corroborate with, so it is kept
    /// forever. This is the pressure valve, never the answer - the fix is
    /// upstream, refusing to write a row that cannot be judged by its own
    /// evidence.
    ///
    /// It is a SEPARATE variant so the sweep can report it separately. Folded
    /// into `Reap`, it silently becomes the main path and turns the
    /// corroboration into decoration.
    ///
    /// It clears the SAME worktree guard `Reap` does. Only the corroboration
    /// requirement is waived here, never the cleanliness one.
    ReapBackstop,
    /// Remove a LIVE row on a positive done reading: idle past the grace window
    /// AND the transcript tail classifies `done` (promise emitted). The session
    /// is idle-and-resumable, not dead, so the reap records a resumable handle
    /// (harness + session id) in the event rather than treating it as a death.
    ///
    /// A separate verdict for the same reason `ReapBackstop` is one: folded into
    /// `Reap`, live-but-done becomes the ordinary route and a death and a
    /// finished turn stop being distinguishable in the counts. A credential-dead
    /// worker whose tail reads anything but `done` is NEITHER alive nor dead -
    /// only the positive done reading evicts, so every other tail state keeps.
    ///
    /// Clears the SAME worktree guard the removal verdicts do: a done worker's
    /// worktree can hold uncommitted work, and dropping the row drops the only
    /// pointer to it.
    ReapDormant,
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
    /// Does the row's session still exist in its OWN harness's store?
    /// `Some(true)` the store entry is GONE (positive evidence the session
    /// ended), `Some(false)` still present, `None` no session id recorded or
    /// the store could not be read.
    ///
    /// Keyed on the row's own harness, never another harness's store: a codex
    /// worker has no claude transcript by construction, so judging it by
    /// claude's store would reap every codex row on the machine. The probe
    /// exists because `claude rm` removes the session from Claude Code while
    /// the registry row survives - the harness store is the authority the
    /// registry cannot see on its own.
    pub harness_session_gone: Option<bool>,
    /// A LIVE row idle past the grace window whose transcript tail classifies
    /// `done`. Set by the sweep only after BOTH the idle gate and the
    /// truth-tail probe answered positively; the one live-row exit from the
    /// registry, reported as [`GcAction::ReapDormant`].
    pub dormant_done: bool,
    /// Worktree cleanliness for a worktree-owning row: `Some(true)` clean,
    /// `Some(false)` dirty (uncommitted changes -> keep), `None` the probe could
    /// not determine it (fail closed -> keep). Ignored when `owns_worktree` is
    /// false, because then there is nothing for it to protect.
    pub worktree_clean: Option<bool>,
}

/// The terminal status set, spelled ONCE: `gc_decide` and the sweep's probe
/// gates both ask this, because the sweep gates its probes on the same
/// terminal-or-dead question the policy will rule on, and two local spellings
/// of one set is how they drifted apart the first time (Orphaned joined the
/// policy's set while the sweep's copy kept the old three).
pub fn status_is_terminal(status: AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::Exited | AgentStatus::PermanentDead | AgentStatus::Orphaned
    )
}

/// Whether this row has at least one POSITIVE, independent reading that its
/// worker is gone. See the corroboration gate in [`gc_action`] for why an
/// absence never qualifies.
///
/// A function rather than an inline expression because the DAEMON must ask the
/// same question before it decides whether to probe the worktree. Gate that
/// probe on anything narrower and a row this call says is reapable never gets
/// its worktree read: `worktree_clean` stays `None`, the fail-closed arm keeps
/// the row, and the `kept_dirty` line that would have named it is gated on the
/// same narrow flag - so the row is stuck AND invisible. Two spellings of one
/// rule is how they drifted apart the first time.
///
/// Ignores `worktree_clean` on purpose: this is the corroboration question
/// alone, asked before any probe has run.
pub fn removal_is_corroborated(row: &GcRow) -> bool {
    row.pid_confirmed_dead
        || row.transcript_fresh == Some(false)
        || row.harness_session_gone == Some(true)
        || !row.liveness_surface
}

/// WHICH gate is holding a [`GcAction::Keep`] row, for diagnostic reporting
/// only (x-9de7 task 5). Never read by the policy itself - a row that is
/// stuck and invisible is the failure mode `gc_action`'s own corroboration
/// gate warns about, and this exists so `fno agents reap --dry-run` can name
/// the gate instead of a silent, unexplained keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepReason {
    /// Liveness re-check reports the row alive right now.
    Live,
    /// Not yet terminal and no confirmed-dead pid: still coming up or running.
    NotTerminal,
    /// Dead, but still inside the grace window since first observed dead.
    WithinGrace,
    /// Past grace, but nothing positively corroborates the worker is gone,
    /// and the row is short of the absolute-age backstop horizon. The
    /// stuck-and-invisible case this reason exists to surface.
    Uncorroborated,
    /// Earned a reap (or the backstop), but the worktree it owns is dirty.
    WorktreeDirty,
    /// Earned a reap (or the backstop), but the worktree cleanliness probe
    /// could not answer (fail-closed).
    WorktreeUnprobed,
}

impl KeepReason {
    /// Stable, human-readable tag for CLI/JSON output.
    pub fn as_str(self) -> &'static str {
        match self {
            KeepReason::Live => "live",
            KeepReason::NotTerminal => "not-terminal",
            KeepReason::WithinGrace => "within-grace",
            KeepReason::Uncorroborated => "uncorroborated",
            KeepReason::WorktreeDirty => "worktree-dirty",
            KeepReason::WorktreeUnprobed => "worktree-unprobed",
        }
    }
}

/// The one decision implementation `gc_action` and `keep_reason` both read
/// from, so the policy is never spelled twice (the exact drift `gc_action`'s
/// own doc comment warns `removal_is_corroborated` against). `KeepReason` is
/// `Some` iff the action is `GcAction::Keep`.
fn gc_decide(row: &GcRow, now: i64, grace_secs: i64) -> (GcAction, Option<KeepReason>) {
    // (AC1-FR) A live worker -- re-checked -- is never touched. The caller clears
    // any stale `exited_at` on such a row separately.
    if row.is_live {
        // The one live-row exit: a POSITIVE done reading (idle past grace,
        // transcript tail classifies `done`). That worker finished its turn;
        // the row leaves as `ReapDormant` with a resumable handle recorded by
        // the caller. Every other live row is untouched on death evidence.
        if !row.dormant_done {
            return (GcAction::Keep, Some(KeepReason::Live));
        }
        return apply_worktree_guard(row, GcAction::ReapDormant);
    }
    // Reap condition #1: terminal status OR a confirmed-dead pid. A non-terminal
    // row with no confirmed-dead pid (e.g. `Spawning` with no pid recorded yet)
    // is NOT eligible -- never reap something still coming up.
    //
    // `Orphaned` is terminal for GC purposes: reconcile parks a row there on an
    // unreachable probe, which is an external observation of death-adjacency,
    // so the row earns a stamp and ages through the same grace + corroboration
    // path as `Exited`. Before this, an Orphaned row with no pid never got an
    // `exited_at` stamp, never entered the grace path, and the backstop (which
    // hangs off `exited_at`) never fired: Orphaned rows were immortal.
    let terminal_or_dead = status_is_terminal(row.status) || row.pid_confirmed_dead;
    if !terminal_or_dead {
        return (GcAction::Keep, Some(KeepReason::NotTerminal));
    }
    match row.exited_at {
        // First observation of a dead row: start the grace clock, do not reap yet.
        None => (GcAction::StampExit, None),
        Some(exited) => {
            // Boundary: keep until STRICTLY past the grace window. A row that
            // exited exactly `grace_secs` ago is still kept.
            if now.saturating_sub(exited) <= grace_secs {
                return (GcAction::Keep, Some(KeepReason::WithinGrace));
            }
            // CORROBORATION GATE. `status` and `exited_at` both derive from one
            // sweep's failure to reach a worker, so they are a single signal
            // wearing two hats and cannot confirm each other. Removal needs at
            // least one POSITIVE, independent reading that the worker is gone.
            //
            // Three qualify. A pid whose start time no longer matches is a
            // process that provably ended. A transcript untouched for the whole
            // window is a session that provably stopped writing. A row with no
            // liveness surface at all never had a worker to lose. A session
            // entry gone from its OWN harness's store is a session that
            // provably ended there (`claude rm` removes the harness record while
            // the registry row survives).
            //
            // None of them available means keep. Reaping a live session destroys
            // work in progress and, worse, the only process able to satisfy its
            // own PR's review gate.
            let independently_gone = removal_is_corroborated(row);
            // Which removal this row has EARNED, before the worktree guard gets
            // its say. Both removals fall through to that one guard below. An
            // early `return` here is the decorative-guard shape: the backstop
            // would skip the dirty-worktree keep AND the fail-closed `None`
            // arm, so one removal path would honour a protection the other
            // silently walks past, and the uncommitted work in that worktree
            // would lose its only pointer.
            let earned = if independently_gone {
                GcAction::Reap
            } else if now.saturating_sub(exited) > backstop_horizon_secs(grace_secs) {
                // ABSOLUTE-AGE BACKSTOP. At this horizon, no signal for that
                // long is itself a signal. Deliberately many multiples of the
                // grace window, so it can never overtake corroboration as the
                // ordinary route out of the registry.
                GcAction::ReapBackstop
            } else {
                return (GcAction::Keep, Some(KeepReason::Uncorroborated));
            };
            apply_worktree_guard(row, earned)
        }
    }
}

/// The worktree guard every removal verdict must clear. Spelled once so a
/// future verdict cannot walk past it: `Reap`, `ReapBackstop`, and `ReapDormant`
/// each drop a row that may be the only pointer at a worktree, and an unguarded
/// removal path would orphan uncommitted work the same way for each.
fn apply_worktree_guard(row: &GcRow, earned: GcAction) -> (GcAction, Option<KeepReason>) {
    if !row.owns_worktree {
        // No worktree to protect, so nothing for cleanliness to say.
        return (earned, None);
    }
    match row.worktree_clean {
        Some(true) => (earned, None),
        // Dirty worktree kept (AC1-EDGE); probe failure fails closed.
        Some(false) => (GcAction::Keep, Some(KeepReason::WorktreeDirty)),
        None => (GcAction::Keep, Some(KeepReason::WorktreeUnprobed)),
    }
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
    gc_decide(row, now, grace_secs).0
}

/// WHICH gate is keeping this row, diagnostic-only (x-9de7 task 5). `None`
/// when the action is not `Keep` (nothing to explain) or the row was just
/// `StampExit`ed (not yet a candidate). Never changes the verdict `gc_action`
/// returns - a pure readout of the same decision, for `fno agents reap
/// --dry-run` to name the gate holding a stuck row instead of reporting it
/// as a silent, unexplained keep.
pub fn keep_reason(row: &GcRow, now: i64, grace_secs: i64) -> Option<KeepReason> {
    gc_decide(row, now, grace_secs).1
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
            harness_session_gone: None,
            dormant_done: false,
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
            harness_session_gone: None,
            dormant_done: false,
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

    // -- Harness-store corroboration (AC1 / AC3 / AC5) ----------------------

    #[test]
    fn a_gone_harness_session_corroborates_on_its_own() {
        // THE `claude rm` CASE: the harness store's entry for the session is
        // gone while the row still reads Exited with a live pid slot and an
        // unreadable transcript. The store is the authority the registry cannot
        // see, and its absence there is positive evidence the session ended.
        let row = GcRow {
            pid_confirmed_dead: false,
            transcript_fresh: None,
            harness_session_gone: Some(true),
            ..reapable()
        };
        assert_eq!(gc_action(&row, NOW, GRACE), GcAction::Reap);
    }

    #[test]
    fn a_present_harness_session_never_corroborates() {
        // Some(false) means the session still exists in its own store: the
        // strongest KEEP-leaning reading there is. It must not corroborate, and
        // it must not override any other signal either (an unresolvable probe
        // answers None, which also never corroborates - AC5).
        let present = GcRow {
            pid_confirmed_dead: false,
            transcript_fresh: None,
            harness_session_gone: Some(false),
            ..reapable()
        };
        assert_eq!(gc_action(&present, NOW, GRACE), GcAction::Keep);
        let unknown = GcRow {
            harness_session_gone: None,
            ..present
        };
        assert_eq!(gc_action(&unknown, NOW, GRACE), GcAction::Keep);
    }

    #[test]
    fn a_gone_harness_session_never_shortcuts_the_earlier_guards() {
        // The store signal is additional, never a replacement: it must not skip
        // liveness, the grace window, or terminal status.
        let live = GcRow {
            is_live: true,
            harness_session_gone: Some(true),
            ..reapable()
        };
        assert_eq!(gc_action(&live, NOW, GRACE), GcAction::Keep);

        let in_grace = GcRow {
            exited_at: Some(NOW - GRACE + 10),
            harness_session_gone: Some(true),
            ..reapable()
        };
        assert_eq!(gc_action(&in_grace, NOW, GRACE), GcAction::Keep);

        let not_terminal = GcRow {
            status: AgentStatus::Spawning,
            exited_at: None,
            harness_session_gone: Some(true),
            ..reapable()
        };
        assert_eq!(gc_action(&not_terminal, NOW, GRACE), GcAction::Keep);
    }

    // -- Orphaned rows age into eviction (AC4) -------------------------------

    #[test]
    fn an_orphaned_row_stamps_and_ages_like_an_exited_one() {
        // The immortal-row mechanism: reconcile parks unreachable rows in
        // Orphaned; before it joined the terminal set such a row never got an
        // `exited_at` stamp, so neither the grace path nor the backstop (which
        // hangs off the stamp) could ever fire.
        let unstamped = GcRow {
            status: AgentStatus::Orphaned,
            pid_confirmed_dead: false,
            exited_at: None,
            ..reapable()
        };
        assert_eq!(gc_action(&unstamped, NOW, GRACE), GcAction::StampExit);

        let in_grace = GcRow {
            status: AgentStatus::Orphaned,
            exited_at: Some(NOW - GRACE + 10),
            ..reapable()
        };
        assert_eq!(gc_action(&in_grace, NOW, GRACE), GcAction::Keep);

        let corroborated = GcRow {
            status: AgentStatus::Orphaned,
            pid_confirmed_dead: false,
            exited_at: Some(NOW - GRACE - 1),
            harness_session_gone: Some(true),
            ..reapable()
        };
        assert_eq!(gc_action(&corroborated, NOW, GRACE), GcAction::Reap);
    }

    #[test]
    fn an_orphaned_row_still_needs_corroboration_past_grace() {
        // Terminal-set membership is the ONLY special case. The same three-gate
        // path applies: no positive death reading, short of the backstop, kept.
        let row = GcRow {
            status: AgentStatus::Orphaned,
            pid_confirmed_dead: false,
            transcript_fresh: None,
            harness_session_gone: None,
            exited_at: Some(NOW - GRACE - 1),
            ..reapable()
        };
        assert_eq!(gc_action(&row, NOW, GRACE), GcAction::Keep);
        assert_eq!(
            keep_reason(&row, NOW, GRACE),
            Some(KeepReason::Uncorroborated)
        );
    }

    // -- Live-but-done rows leave as dormant (AC7) ---------------------------

    #[test]
    fn a_live_row_with_a_done_tail_is_reaped_as_dormant() {
        // A bg thread that emitted its promise: idle past grace, tail reads
        // done. It leaves the view, resumable handle recorded by the sweep.
        let row = GcRow {
            is_live: true,
            dormant_done: true,
            owns_worktree: false,
            ..reapable()
        };
        assert_eq!(gc_action(&row, NOW, GRACE), GcAction::ReapDormant);
        // NOT folded into Reap: a death and a finished turn must stay
        // distinguishable in the counts (the same reason ReapBackstop is
        // separate).
        assert_ne!(gc_action(&row, NOW, GRACE), GcAction::Reap);
    }

    #[test]
    fn a_dormant_reap_clears_the_same_worktree_guard() {
        // A done worker's worktree can hold uncommitted work; dropping the row
        // drops the only pointer to it. Dirty keeps, unprobed fails closed.
        let dirty = GcRow {
            is_live: true,
            dormant_done: true,
            owns_worktree: true,
            worktree_clean: Some(false),
            ..reapable()
        };
        assert_eq!(gc_action(&dirty, NOW, GRACE), GcAction::Keep);
        assert_eq!(
            keep_reason(&dirty, NOW, GRACE),
            Some(KeepReason::WorktreeDirty)
        );

        let unprobed = GcRow {
            worktree_clean: None,
            ..dirty
        };
        assert_eq!(gc_action(&unprobed, NOW, GRACE), GcAction::Keep);
    }

    #[test]
    fn a_live_row_without_a_done_tail_is_kept_whatever_the_death_evidence() {
        // The credential-dead specimen: reads live, transcript idle for an
        // hour. Neither alive nor dead - only a POSITIVE done reading evicts,
        // and every other tail state keeps.
        let row = GcRow {
            is_live: true,
            dormant_done: false,
            pid_confirmed_dead: true,
            transcript_fresh: Some(false),
            harness_session_gone: Some(true),
            ..reapable()
        };
        assert_eq!(gc_action(&row, NOW, GRACE), GcAction::Keep);
        assert_eq!(keep_reason(&row, NOW, GRACE), Some(KeepReason::Live));
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

    // -- The absolute-age backstop -----------------------------------------
    //
    // A row with an identity but no pid and no transcript offers nothing to
    // corroborate with, so the fail-closed gate keeps it forever and the
    // registry grows without bound. These three pin the valve: it opens only at
    // the far horizon, it is a DISTINCT verdict so the sweep can report it
    // apart from corroborated reaps, and it never front-runs corroboration.

    /// Uncorroborated: identity recorded, pid unknown, transcript unknown.
    fn uncorroborated(exited_at: i64) -> GcRow {
        GcRow {
            pid_confirmed_dead: false,
            liveness_surface: true,
            transcript_fresh: None,
            exited_at: Some(exited_at),
            ..reapable()
        }
    }

    #[test]
    fn uncorroborated_row_is_kept_before_the_backstop_horizon() {
        // Just past grace, nowhere near the horizon. This is the case the
        // corroboration gate exists for, and the backstop must not shorten it.
        assert_eq!(
            gc_action(&uncorroborated(NOW - GRACE - 1), NOW, GRACE),
            GcAction::Keep
        );
        // One second short of the horizon: still kept. Without this, an
        // off-by-a-window backstop would pass the test below unnoticed.
        let edge = NOW - GRACE * BACKSTOP_GRACE_MULTIPLE;
        assert_eq!(gc_action(&uncorroborated(edge), NOW, GRACE), GcAction::Keep);
    }

    #[test]
    fn uncorroborated_row_past_the_horizon_reaps_as_backstop() {
        let past = NOW - GRACE * BACKSTOP_GRACE_MULTIPLE - 1;
        // NOT `Reap`. A backstop folded into the corroborated verdict becomes
        // the main path silently and turns the gate into decoration.
        assert_eq!(
            gc_action(&uncorroborated(past), NOW, GRACE),
            GcAction::ReapBackstop
        );
    }

    #[test]
    fn a_corroborated_row_never_reports_as_backstop() {
        // Same far-past age, but with a positive signal. The ordinary verdict
        // must win, or the two counts stop meaning what they say.
        let past = NOW - GRACE * BACKSTOP_GRACE_MULTIPLE - 1;
        let row = GcRow {
            transcript_fresh: Some(false),
            exited_at: Some(past),
            ..reapable()
        };
        assert_eq!(gc_action(&row, NOW, GRACE), GcAction::Reap);
    }

    // -- The backstop clears the same worktree guard `Reap` does ------------
    //
    // The backstop waives CORROBORATION. It waives nothing else. Returning it
    // above these two arms is the decorative-guard shape: the dirty-worktree
    // keep and the fail-closed probe arm would protect one removal path and be
    // skipped by the other, and the uncommitted work in that worktree would be
    // orphaned with no registry row left pointing at it.

    #[test]
    fn backstop_does_not_reap_a_dirty_worktree() {
        let past = NOW - backstop_horizon_secs(GRACE) - 1;
        let row = GcRow {
            worktree_clean: Some(false),
            ..uncorroborated(past)
        };
        assert_eq!(gc_action(&row, NOW, GRACE), GcAction::Keep);
    }

    #[test]
    fn backstop_fails_closed_when_the_cleanliness_probe_cannot_answer() {
        let past = NOW - backstop_horizon_secs(GRACE) - 1;
        let row = GcRow {
            worktree_clean: None,
            ..uncorroborated(past)
        };
        assert_eq!(gc_action(&row, NOW, GRACE), GcAction::Keep);
    }

    #[test]
    fn backstop_still_reaps_a_row_that_owns_no_worktree() {
        // The guard above must not become a blanket refusal: a row with no
        // worktree has nothing for cleanliness to protect, so the valve opens.
        let past = NOW - backstop_horizon_secs(GRACE) - 1;
        let row = GcRow {
            owns_worktree: false,
            worktree_clean: None,
            ..uncorroborated(past)
        };
        assert_eq!(gc_action(&row, NOW, GRACE), GcAction::ReapBackstop);
    }

    // -- The probe condition and the removal condition are one rule ---------

    #[test]
    fn corroboration_agrees_with_the_verdict_on_every_signal_combination() {
        // The daemon gates its worktree probe on `removal_is_corroborated`. If
        // the two ever answer differently, a row the policy would reap never
        // gets probed, `worktree_clean` stays None, the fail-closed arm keeps it
        // forever, and `kept_dirty` (gated on the same flag) names nothing. So
        // assert the equivalence directly, across all 12 signal combinations.
        let past = NOW - GRACE - 1;
        for &pid_dead in &[true, false] {
            for &fresh in &[Some(true), Some(false), None] {
                for &gone in &[Some(true), Some(false), None] {
                    for &surface in &[true, false] {
                        let row = GcRow {
                            pid_confirmed_dead: pid_dead,
                            transcript_fresh: fresh,
                            harness_session_gone: gone,
                            liveness_surface: surface,
                            exited_at: Some(past),
                            owns_worktree: false,
                            worktree_clean: None,
                            ..reapable()
                        };
                        let corroborated = removal_is_corroborated(&row);
                        // Just past grace, far short of the horizon, so the ONLY
                        // route to a removal here is corroboration.
                        let reaped = gc_action(&row, NOW, GRACE) == GcAction::Reap;
                        assert_eq!(
                            corroborated, reaped,
                            "probe and verdict disagree for pid_dead={pid_dead} \
                             fresh={fresh:?} gone={gone:?} surface={surface}"
                        );
                    }
                }
            }
        }
    }

    // -- The horizon has an absolute floor ----------------------------------

    #[test]
    fn a_zero_grace_does_not_collapse_the_horizon() {
        // `agents.dead_row_grace = 0` reaches here unclamped. Multiplied alone
        // the horizon is 0, and every uncorroborated row reaps one tick later -
        // the gate inverted by a config scalar. A day old is still kept.
        let row = uncorroborated(NOW - 86_400);
        assert_eq!(gc_action(&row, NOW, 0), GcAction::Keep);
        assert_eq!(backstop_horizon_secs(0), BACKSTOP_MIN_HORIZON_SECS);
    }

    #[test]
    fn the_floor_never_shortens_a_larger_configured_horizon() {
        // The floor is a minimum, not a cap. A grace larger than the default
        // must still multiply out past it.
        let big = GRACE * 10;
        assert_eq!(
            backstop_horizon_secs(big),
            big * BACKSTOP_GRACE_MULTIPLE,
            "the floor overrode a horizon that was already longer"
        );
        // And the default config lands exactly on the floor, so nothing moved.
        assert_eq!(backstop_horizon_secs(GRACE), BACKSTOP_MIN_HORIZON_SECS);
    }

    // -- keep_reason (x-9de7 task 5): diagnostic-only, must never move gc_action

    #[test]
    fn keep_reason_agrees_with_gc_action_on_every_fixture_in_this_file() {
        // The single strongest guarantee this function needs: `Some(_)` iff
        // `GcAction::Keep`, on every fixture already exercised above, so the
        // diagnostic can never say Keep while the policy reaps (or vice
        // versa). Reuses the exact combinatorial sweep already proven to
        // cover the policy's branches.
        let past = NOW - GRACE - 1;
        for &pid_dead in &[true, false] {
            for &fresh in &[Some(true), Some(false), None] {
                for &gone in &[Some(true), Some(false), None] {
                    for &surface in &[true, false] {
                        for &live in &[true, false] {
                            for &dormant in &[true, false] {
                                for &wt_clean in &[Some(true), Some(false), None] {
                                    let row = GcRow {
                                        is_live: live,
                                        dormant_done: dormant,
                                        pid_confirmed_dead: pid_dead,
                                        transcript_fresh: fresh,
                                        harness_session_gone: gone,
                                        liveness_surface: surface,
                                        exited_at: Some(past),
                                        owns_worktree: true,
                                        worktree_clean: wt_clean,
                                        ..reapable()
                                    };
                                    let action = gc_action(&row, NOW, GRACE);
                                    let reason = keep_reason(&row, NOW, GRACE);
                                    assert_eq!(
                                        action == GcAction::Keep,
                                        reason.is_some(),
                                        "action={action:?} reason={reason:?} for \
                                         live={live} dormant={dormant} pid_dead={pid_dead} \
                                         fresh={fresh:?} gone={gone:?} surface={surface} \
                                         wt_clean={wt_clean:?}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn keep_reason_is_none_for_a_stamp_exit() {
        // A row seen dead for the first time is StampExit'd, not Keep: there
        // is nothing yet to explain.
        let row = GcRow {
            exited_at: None,
            ..reapable()
        };
        assert_eq!(gc_action(&row, NOW, GRACE), GcAction::StampExit);
        assert_eq!(keep_reason(&row, NOW, GRACE), None);
    }

    #[test]
    fn keep_reason_names_live() {
        let row = GcRow {
            is_live: true,
            ..reapable()
        };
        assert_eq!(keep_reason(&row, NOW, GRACE), Some(KeepReason::Live));
    }

    #[test]
    fn keep_reason_names_not_terminal() {
        let row = GcRow {
            status: AgentStatus::Idle,
            pid_confirmed_dead: false,
            ..reapable()
        };
        assert_eq!(keep_reason(&row, NOW, GRACE), Some(KeepReason::NotTerminal));
    }

    #[test]
    fn keep_reason_names_within_grace() {
        let row = GcRow {
            exited_at: Some(NOW - GRACE + 10),
            ..reapable()
        };
        assert_eq!(keep_reason(&row, NOW, GRACE), Some(KeepReason::WithinGrace));
    }

    #[test]
    fn keep_reason_names_uncorroborated_the_stuck_and_invisible_case() {
        // The exact case task 5 exists for: past grace, nothing positively
        // corroborates death, short of the backstop horizon.
        let row = uncorroborated(NOW - GRACE - 1);
        assert_eq!(gc_action(&row, NOW, GRACE), GcAction::Keep);
        assert_eq!(
            keep_reason(&row, NOW, GRACE),
            Some(KeepReason::Uncorroborated)
        );
    }

    #[test]
    fn keep_reason_names_worktree_dirty_and_worktree_unprobed_distinctly() {
        let dirty = GcRow {
            worktree_clean: Some(false),
            ..reapable()
        };
        assert_eq!(
            keep_reason(&dirty, NOW, GRACE),
            Some(KeepReason::WorktreeDirty)
        );
        let unprobed = GcRow {
            worktree_clean: None,
            ..reapable()
        };
        assert_eq!(
            keep_reason(&unprobed, NOW, GRACE),
            Some(KeepReason::WorktreeUnprobed)
        );
    }

    #[test]
    fn keep_reason_as_str_is_pairwise_distinct() {
        // The CLI/JSON tag: every variant must read differently, or two
        // distinct stuck reasons collapse into one string an operator cannot
        // tell apart.
        let all = [
            KeepReason::Live,
            KeepReason::NotTerminal,
            KeepReason::WithinGrace,
            KeepReason::Uncorroborated,
            KeepReason::WorktreeDirty,
            KeepReason::WorktreeUnprobed,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a.as_str(), b.as_str());
            }
        }
    }
}
