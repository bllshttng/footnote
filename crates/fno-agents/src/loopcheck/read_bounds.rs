//! The fire's read bounds: one wall-clock ceiling per stop-gate read, one
//! aggregate budget per fire, and the drain's reserved slice. One place so
//! the bounds cannot drift apart or outlive the stop hook's kill.

/// Wall-clock ceiling for ONE synchronous stop-gate read: PR metadata,
/// checks, reviews, inline comments, commits, quota, coverage reads,
/// fingerprint reads, nudges/coverage writes, the king board, and every
/// local `git` read the gate makes. One named bound so a single wedged child
/// can never outlive the stop fire; the fidelity ceiling (60s) and the
/// advisory-hint ceiling (10s) stay separate because they gate different
/// things and drift independently.
const STOPGATE_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Aggregate wall-clock budget for ONE fire's external reads. The harness
/// kills the stop hook at 60s (hooks/git-protection.py records the budget and
/// the Stop hook carries no override), so N sequential reads each legal at
/// 30s could burn multiples of that and die mid-fire with no decision -
/// recreating the exact no-decision path this transport exists to close.
/// Reads that start after the budget is spent still run, but at the floor
/// bound, so the fire ends fast with a decision naming what it could not
/// wait for.
pub(crate) const STOPGATE_FIRE_BUDGET: std::time::Duration = std::time::Duration::from_secs(50);

/// A spent budget must still bound every remaining read, so the effective
/// ceiling never reaches zero: this floor keeps a late read killable in
/// bounded time instead of degenerating into an unbounded wait.
pub(crate) const STOPGATE_BOUND_FLOOR: std::time::Duration = std::time::Duration::from_millis(250);

/// King fires hold this much of the fire budget back for the drain read, the
/// last read and the one that decides completion. Drain cost scales with
/// graph rows: measured standalone 4.5s to 7.9s on one scope and 8.8s to
/// 11.2s on the largest. 16s is 1.4x that worst measurement; the multiplier
/// is a judgment call, not a measured bound, and no larger scope has been
/// measured yet. The drain's own ceiling is the wider `min(30s, remaining)`
/// (`stopgate_drain_timeout`): the reserve guarantees only the floor a
/// starved fire still leaves, which is what turns a spent budget from a
/// silent 250ms kill into a readable timeout. Every cheaper read before the
/// drain is clamped to `remaining - reserve`.
const STOPGATE_DRAIN_RESERVE: std::time::Duration = std::time::Duration::from_secs(16);

thread_local! {
    /// The fire's read-bound override (from `--read-timeout-ms`, 0 meaning
    /// the production default), its budget deadline, and the drain reserve
    /// (0 when the driver has no drain), stamped when `decide` begins.
    /// Thread-local, not process-global: the test harness fires
    /// `decide` in-process on parallel threads, and one fire must never reset
    /// or inherit a neighboring fire's injected bound mid-read.
    static STOPGATE_READS: std::cell::RefCell<(u64, Option<std::time::Instant>, u64)> =
        const { std::cell::RefCell::new((0, None, 0)) };
}

/// The reserve a king fire holds back, in ms, for the deciding drain read.
pub(crate) fn stopgate_drain_reserve_ms() -> u64 {
    STOPGATE_DRAIN_RESERVE.as_millis() as u64
}

/// Stamp this fire's read-bound override, budget deadline, and drain reserve
/// before any read can fire. Thread-local, not process-global: the test
/// harness fires `decide` in-process on parallel threads, and one fire must
/// never reset or inherit a neighboring fire's injected bound mid-read.
pub(crate) fn stopgate_stamp_fire(override_ms: u64, deadline: std::time::Instant, reserve_ms: u64) {
    STOPGATE_READS.with(|cell| {
        *cell.borrow_mut() = (override_ms, Some(deadline), reserve_ms);
    });
}

/// The configured ceiling for one read: the flag override or the production
/// default.
fn stopgate_configured_timeout(override_ms: u64) -> std::time::Duration {
    if override_ms > 0 {
        std::time::Duration::from_millis(override_ms)
    } else {
        STOPGATE_READ_TIMEOUT
    }
}

/// Effective bound for one stop-gate read: the configured ceiling (flag or
/// production default) clamped to whatever remains of this fire's aggregate
/// budget minus the drain reserve, floored so the answer is always a
/// positive killable bound.
pub(crate) fn stopgate_read_timeout() -> std::time::Duration {
    STOPGATE_READS.with(|cell| {
        let (override_ms, deadline, reserve_ms) = *cell.borrow();
        let configured = stopgate_configured_timeout(override_ms);
        match deadline {
            Some(d) => {
                let remaining = d.saturating_duration_since(std::time::Instant::now());
                let for_pre_drain =
                    remaining.saturating_sub(std::time::Duration::from_millis(reserve_ms));
                clamp_to_fire_budget(configured, for_pre_drain)
            }
            None => configured,
        }
    })
}

/// The drain read's bound: the reserved read measures against the fire's
/// full remaining budget - the reserve is what every OTHER read held back
/// for it.
pub(crate) fn stopgate_drain_timeout() -> std::time::Duration {
    STOPGATE_READS.with(|cell| {
        let (override_ms, deadline, _) = *cell.borrow();
        let configured = stopgate_configured_timeout(override_ms);
        match deadline {
            Some(d) => {
                let remaining = d.saturating_duration_since(std::time::Instant::now());
                clamp_to_fire_budget(configured, remaining)
            }
            None => configured,
        }
    })
}

/// Clamp a configured read ceiling to this fire's budget deadline, so no
/// single long read can spend more than the fire still has. The fidelity
/// probe's separate 60s ceiling clamps through here - the budget is the
/// fire's, not the read's.
pub(crate) fn clamp_to_fire_deadline(configured: std::time::Duration) -> std::time::Duration {
    STOPGATE_READS.with(|cell| match cell.borrow().1 {
        Some(d) => {
            let remaining = d.saturating_duration_since(std::time::Instant::now());
            clamp_to_fire_budget(configured, remaining)
        }
        None => configured,
    })
}

/// Pure clamp so the deadline math is testable without a clock: never above
/// what remains of the budget, never below the floor.
pub(crate) fn clamp_to_fire_budget(
    configured: std::time::Duration,
    remaining: std::time::Duration,
) -> std::time::Duration {
    configured.min(remaining).max(STOPGATE_BOUND_FLOOR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_drain_reserve_holds_pre_drain_reads_back_and_spares_the_drain() {
        // King fires stamp the reserve; every read before the drain sees
        // remaining-minus-reserve, the drain sees the full remaining.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        STOPGATE_READS.with(|cell| {
            *cell.borrow_mut() = (0, Some(deadline), STOPGATE_DRAIN_RESERVE.as_millis() as u64);
        });
        // Pre-drain read: 30s ceiling clamped to 20s minus the reserve,
        // minus only the microseconds between the stamp and the asserts.
        let expected = std::time::Duration::from_secs(20) - STOPGATE_DRAIN_RESERVE;
        let pre_drain = stopgate_read_timeout();
        assert!(
            pre_drain <= expected && pre_drain >= expected - std::time::Duration::from_secs(1),
            "{pre_drain:?}"
        );
        // The reserved read itself: the full 20s.
        let reserved = stopgate_drain_timeout();
        assert!(
            reserved <= std::time::Duration::from_secs(20)
                && reserved >= std::time::Duration::from_secs(19),
            "{reserved:?}"
        );
    }

    #[test]
    fn a_spent_budget_floors_both_bounds_still_killable() {
        let deadline = std::time::Instant::now();
        STOPGATE_READS.with(|cell| {
            *cell.borrow_mut() = (0, Some(deadline), STOPGATE_DRAIN_RESERVE.as_millis() as u64);
        });
        assert_eq!(stopgate_read_timeout(), STOPGATE_BOUND_FLOOR);
        assert_eq!(stopgate_drain_timeout(), STOPGATE_BOUND_FLOOR);
    }
}
