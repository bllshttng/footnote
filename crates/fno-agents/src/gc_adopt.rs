//! The non-spawn rows that may leave the keep-forever lane: the corpse
//! probe and the adopted-retire carve-out, the two predicates the sweep's
//! origin pre-gate and staging filters apply. Split out of `gc_sweep.rs`
//! (file budget); both read the same roster snapshot seam, so "what counts
//! as absent" cannot diverge between them.

use crate::claude_roster::ClaudeAgentsSnapshot;
use crate::state;

/// An adopted row keeps only while there is a session to own it. Two
/// positive markers say a row is a registry corpse, and only they let the
/// origin gate skip the row: a recorded pid that answered ESRCH, or a
/// claude row provably absent from a KNOWN roster snapshot (the same
/// predicate the `rm` live gate applies, so "what counts as absent" cannot
/// diverge between the two call sites). An unknown snapshot, a partial
/// list, a missing pid that answers nothing: each keeps the row - absence
/// alone never authorizes a reap. The snapshot is a subprocess read, so
/// the roster leg fires only for a row quiet past the grace: a fresh
/// adopted row cannot pass a later gate anyway, and keeps without the
/// read, exactly as before.
pub(crate) fn origin_corpse(
    e: &state::RegistryEntry,
    quiet_past_grace: bool,
    agents_memo: &std::cell::RefCell<Option<ClaudeAgentsSnapshot>>,
    agents_read: &dyn Fn() -> ClaudeAgentsSnapshot,
) -> bool {
    if e.pid.is_some_and(crate::daemon::pid_is_gone) {
        return true;
    }
    if quiet_past_grace && e.harness_name() == "claude" {
        let mut memo = agents_memo.borrow_mut();
        let snapshot = memo.get_or_insert_with(|| agents_read());
        return crate::daemon::roster_death::claude_row_provably_absent(
            Some(snapshot),
            crate::daemon::roster_death::claude_row_id(e).as_deref(),
        );
    }
    false
}

/// The adopted-retire carve-out, resolved once per row and shared by the
/// staging filter (pass 1) and the origin pre-gate (pass 2): an adopted
/// row whose registry status reads terminal and that no known roster
/// snapshot lists live takes the normal pipeline - the open-PR hold and
/// the grace window judge it - instead of keeping as a phantom forever.
/// A roster-listed session keeps: the listing is the live fact and the
/// registry status the stale one. A non-claude row answers true (no
/// roster instrument governs it), which does not block the carve-out.
pub(crate) fn adopted_row_is_finished(
    e: &state::RegistryEntry,
    agents_memo: &std::cell::RefCell<Option<ClaudeAgentsSnapshot>>,
    agents_read: &dyn Fn() -> ClaudeAgentsSnapshot,
) -> bool {
    if e.origin.as_deref() != Some("adopted") {
        return false;
    }
    if !matches!(
        e.status,
        crate::AgentStatus::Exited | crate::AgentStatus::PermanentDead
    ) {
        return false;
    }
    if e.harness_name() != "claude" {
        return true;
    }
    let mut memo = agents_memo.borrow_mut();
    let snapshot = memo.get_or_insert_with(|| agents_read());
    match crate::daemon::claude_row_id(e) {
        Some(rid) => match snapshot {
            ClaudeAgentsSnapshot::Known { rows, .. } => !rows.iter().any(|r| r.short_id == rid),
            ClaudeAgentsSnapshot::Unknown { .. } => true,
        },
        None => true,
    }
}
