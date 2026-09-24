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

/// What a pre-drain read gets once the reserve line is breached. The 250ms
/// floor cannot apply here: a floored read spends part of the drain's slice,
/// and enough of them spend it whole (measured: a floored drain at 271ms
/// after roughly 64 such reads). Refusing fast keeps the aggregate erosion
/// near zero, and the bound stays positive so the read is still killable.
pub(crate) const STOPGATE_PRE_DRAIN_SPENT_BOUND: std::time::Duration =
    std::time::Duration::from_millis(1);

/// King fires hold this much of the fire budget back for the drain read, the
/// last read and the one that decides completion. Drain cost scales with
/// graph rows: measured standalone 4.5s to 7.9s on one scope and 8.8s to
/// 11.2s on the largest. 16s is 1.4x that worst measurement; the multiplier
/// is a judgment call, not a measured bound, and no larger scope has been
/// measured yet. The drain's own ceiling is the wider `min(30s, remaining)`
/// (`stopgate_drain_timeout`): the reserve guarantees only the floor a
/// starved fire still leaves, which is what turns a spent budget from a
/// silent 250ms kill into a readable timeout. Every cheaper read before the
/// drain is clamped to `remaining - reserve` and refuses fast past that
/// line, so the slice survives any number of pre-drain reads.
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

/// Re-arm the drain reserve on a fire that reached `king_decide` stamped with
/// reserve 0. The Crown route (a bound harness session whose row is crowned)
/// enters the king path under a fire the `--driver` string called target, and
/// the entry stamp is driver-blind now precisely so the route, not the
/// string, decides. Mutates the already-stamped fire; override and deadline
/// stay.
pub(crate) fn stopgate_hold_drain_reserve() {
    STOPGATE_READS.with(|cell| {
        cell.borrow_mut().2 = STOPGATE_DRAIN_RESERVE.as_millis() as u64;
    });
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
/// budget minus the drain reserve. Past the reserve line the read refuses
/// fast instead of taking the floor: the floor is per-read and the erosion
/// is aggregate, so a floored read here spends the drain's reserved slice.
pub(crate) fn stopgate_read_timeout() -> std::time::Duration {
    STOPGATE_READS.with(|cell| {
        let (override_ms, deadline, reserve_ms) = *cell.borrow();
        let configured = stopgate_configured_timeout(override_ms);
        match deadline {
            Some(d) => {
                let remaining = d.saturating_duration_since(std::time::Instant::now());
                let for_pre_drain =
                    remaining.saturating_sub(std::time::Duration::from_millis(reserve_ms));
                if for_pre_drain.is_zero() {
                    STOPGATE_PRE_DRAIN_SPENT_BOUND
                } else {
                    configured.min(for_pre_drain)
                }
            }
            None => configured,
        }
    })
}

/// The drain's own floor: the smallest bound a drain read can still meet.
/// `fno agents king drain` answers in 1.5s to 1.7s warm on this machine
/// (the bare CLI cold start alone costs 1.36s), so the generic 250ms floor
/// was five to seven times under the cost of STARTING the drain - a
/// deterministic kill no cache warmth or quiet fire could pass. 5s is about
/// 3x the measured drain; every non-drain read keeps the 250ms floor.
pub(crate) const STOPGATE_DRAIN_FLOOR: std::time::Duration = std::time::Duration::from_secs(5);

/// How far past the fire budget a drain may reach: the harness kills the
/// stop hook at 60s and the fire budget is 50s, so 10s of real margin
/// exist; 8s keeps 2s for the decision write. Inside that margin the drain
/// keeps its floor even when the budget is spent.
pub(crate) const STOPGATE_HARNESS_MARGIN: std::time::Duration = std::time::Duration::from_secs(8);

/// The drain read's bound: the reserved read measures against the fire's
/// full remaining budget - the reserve is what every OTHER read held back
/// for it - and keeps a floor it can actually meet for as long as the
/// harness kill still leaves the decision writable.
pub(crate) fn stopgate_drain_timeout() -> std::time::Duration {
    STOPGATE_READS.with(|cell| {
        let (override_ms, deadline, _) = *cell.borrow();
        let configured = stopgate_configured_timeout(override_ms);
        match deadline {
            Some(d) => {
                let now = std::time::Instant::now();
                let remaining = d.saturating_duration_since(now);
                // An explicit --read-timeout-ms is an instruction, not a
                // default: tests inject a small bound to force a kill, and
                // the floor must not talk them out of it. The floor repairs
                // the production DEFAULT ceiling only (override_ms == 0).
                if override_ms > 0 {
                    return clamp_to_fire_budget(configured, remaining);
                }
                let hard_remaining = d
                    .checked_add(STOPGATE_HARNESS_MARGIN)
                    .map(|hard| hard.saturating_duration_since(now))
                    .unwrap_or_default();
                std::cmp::max(
                    configured.min(remaining),
                    STOPGATE_DRAIN_FLOOR.min(hard_remaining),
                )
                .max(STOPGATE_BOUND_FLOOR)
            }
            None => configured,
        }
    })
}

/// True when this fire's pre-drain reads are past the reserve line: the
/// board (and every pre-drain read) refuses instead of reading at the 1ms
/// spent bound.
pub(crate) fn stopgate_pre_drain_spent() -> bool {
    STOPGATE_READS.with(|cell| {
        let (_, deadline, reserve_ms) = *cell.borrow();
        match deadline {
            Some(d) => d
                .saturating_duration_since(std::time::Instant::now())
                .saturating_sub(std::time::Duration::from_millis(reserve_ms))
                .is_zero(),
            None => false,
        }
    })
}

/// What remains of this fire's harness margin at the moment of the call, in
/// ms: how much past-budget reach the drain still has before the generic
/// floor applies. 0 when no fire is stamped.
pub(crate) fn stopgate_harness_margin_remaining_ms() -> u64 {
    STOPGATE_READS.with(|cell| match cell.borrow().1 {
        Some(d) => d
            .checked_add(STOPGATE_HARNESS_MARGIN)
            .map(|hard| {
                hard.saturating_duration_since(std::time::Instant::now())
                    .as_millis() as u64
            })
            .unwrap_or(0),
        None => 0,
    })
}

/// What remains of this fire's aggregate budget at the moment of the call,
/// in ms: the spend side of the drain instrumentation, captured when the
/// drain starts. 0 when no fire is stamped, which only non-stop-gate
/// callers see.
pub(crate) fn stopgate_fire_remaining_ms() -> u64 {
    STOPGATE_READS.with(|cell| match cell.borrow().1 {
        Some(d) => d
            .saturating_duration_since(std::time::Instant::now())
            .as_millis() as u64,
        None => 0,
    })
}

/// The drain instrumentation's early-warning line: the reserved read spent
/// at least half the reserve that was held back for it.
pub(crate) fn drain_reserve_half_spent(elapsed: std::time::Duration) -> bool {
    elapsed >= STOPGATE_DRAIN_RESERVE / 2
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
    fn holding_the_reserve_on_a_reserve_zero_fire_arms_the_drain_slice() {
        // AC3: the entry stamp is reserve-blind (0) the way every fire is
        // stamped now; the hold on `king_decide` arms the drain slice, so a
        // Crown-route fire gets exactly what a native king fire always had.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(40);
        STOPGATE_READS.with(|cell| {
            *cell.borrow_mut() = (0, Some(deadline), 0);
        });
        stopgate_hold_drain_reserve();
        // Pre-drain reads clamp to remaining-minus-reserve (~24s left).
        let pre_drain = stopgate_read_timeout();
        assert!(
            pre_drain <= std::time::Duration::from_secs(24)
                && pre_drain >= std::time::Duration::from_secs(23),
            "{pre_drain:?}"
        );
        // The drain reads the full remaining (~40s), never the floor.
        let reserved = stopgate_drain_timeout();
        assert!(
            reserved <= std::time::Duration::from_secs(40)
                && reserved >= std::time::Duration::from_secs(39),
            "{reserved:?}"
        );
    }

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
    fn a_spent_budget_refuses_pre_drain_reads_fast_and_floors_the_drain() {
        let deadline = std::time::Instant::now();
        STOPGATE_READS.with(|cell| {
            *cell.borrow_mut() = (0, Some(deadline), STOPGATE_DRAIN_RESERVE.as_millis() as u64);
        });
        assert_eq!(stopgate_read_timeout(), STOPGATE_PRE_DRAIN_SPENT_BOUND);
        // The drain keeps a floor it can meet: a fresh 30s ceiling collapsed
        // to the deadline, but the harness margin is unspent at the line, so
        // the drain floor still applies - never the 250ms generic floor that
        // no CLI cold start could pass.
        assert_eq!(stopgate_drain_timeout(), STOPGATE_DRAIN_FLOOR);
    }

    #[test]
    fn a_drain_past_the_harness_margin_falls_to_the_generic_floor() {
        // 9s past the budget deadline the 8s harness margin is gone: the
        // generic 250ms floor applies so the hook still answers before the
        // 60s harness kill, leaving ~2s for the decision write.
        let deadline = std::time::Instant::now() - std::time::Duration::from_secs(9);
        STOPGATE_READS.with(|cell| {
            *cell.borrow_mut() = (0, Some(deadline), STOPGATE_DRAIN_RESERVE.as_millis() as u64);
        });
        assert_eq!(stopgate_drain_timeout(), STOPGATE_BOUND_FLOOR);
    }

    #[test]
    fn the_drain_row_fires_only_once_half_the_reserve_is_gone() {
        let reserve_ms = STOPGATE_DRAIN_RESERVE.as_millis() as u64;
        assert!(!drain_reserve_half_spent(std::time::Duration::from_millis(
            reserve_ms / 2 - 1
        )));
        assert!(drain_reserve_half_spent(std::time::Duration::from_millis(
            reserve_ms / 2
        )));
        assert!(drain_reserve_half_spent(STOPGATE_DRAIN_RESERVE));
    }

    #[test]
    fn the_spend_side_reads_the_stamped_deadline() {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        STOPGATE_READS.with(|cell| {
            *cell.borrow_mut() = (0, Some(deadline), STOPGATE_DRAIN_RESERVE.as_millis() as u64);
        });
        let remaining = stopgate_fire_remaining_ms();
        assert!(remaining <= 20_000 && remaining >= 19_000, "{remaining}");
    }

    #[test]
    fn a_breached_reserve_line_stops_eroding_into_the_drain_slice() {
        // The measured specimen: remaining == reserve, so a floored
        // pre-drain read would take 250ms of the drain's slice and a few
        // dozen of them would spend it whole. The refuse-fast bound spends
        // ~nothing, and the drain still reads the full remaining.
        let deadline = std::time::Instant::now() + STOPGATE_DRAIN_RESERVE;
        STOPGATE_READS.with(|cell| {
            *cell.borrow_mut() = (0, Some(deadline), STOPGATE_DRAIN_RESERVE.as_millis() as u64);
        });
        let pre_drain = stopgate_read_timeout();
        assert!(
            pre_drain <= std::time::Duration::from_millis(5),
            "{pre_drain:?}"
        );
        let reserved = stopgate_drain_timeout();
        assert!(
            reserved <= STOPGATE_DRAIN_RESERVE
                && reserved >= STOPGATE_DRAIN_RESERVE - std::time::Duration::from_secs(1),
            "{reserved:?}"
        );
    }
}
