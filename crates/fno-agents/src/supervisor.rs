//! Restart policy state machine (design module `supervisor.rs`).
//!
//! After a PTY-managed agent's child crashes (post-spawn; pre-spawn validation
//! failures never restart, LD32), the policy decides whether to re-spawn and
//! how long to back off. LD36 imposes a hard ceiling: `consecutive_failures >=
//! 10` triggers `permanent_dead` regardless of any provider-supplied policy.
//! The provider's `default_restart_policy()` is capped at this ceiling so a
//! buggy provider cannot request infinite restarts.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// The hard ceiling from LD36. No provider policy may exceed it.
pub const HARD_FAILURE_CEILING: u32 = 10;

/// Backoff schedule between restart attempts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Backoff {
    /// Delay before the first restart.
    pub base: Duration,
    /// Multiplier applied per consecutive failure (exponential when > 1).
    pub factor_milli: u32, // factor * 1000, to keep the struct serde-trivial
    /// Cap on any single backoff delay.
    pub max: Duration,
}

impl Backoff {
    /// Delay for the Nth consecutive failure (1-based). `failures == 1` yields
    /// `base`; each subsequent failure multiplies by `factor`, capped at `max`.
    pub fn delay_for(&self, consecutive_failures: u32) -> Duration {
        // Compute the raw (pre-floor) delay in ms for every path, then apply a
        // single unconditional 1ms floor at the end. Flooring in ONE place
        // covers both the first-failure path AND the exponential path; an
        // earlier version floored only the latter, so a sub-millisecond `base`
        // on the first failure slipped through.
        let raw_ms = if consecutive_failures <= 1 {
            self.base.min(self.max).as_millis() as f64
        } else {
            let factor = (self.factor_milli as f64) / 1000.0;
            let exp = (consecutive_failures - 1) as i32;
            let base_ms = self.base.as_millis() as f64;
            let scaled_ms = base_ms * factor.powi(exp);
            let capped_ms = scaled_ms.min(self.max.as_millis() as f64);
            // Guard against NaN/inf from pathological factors.
            if !capped_ms.is_finite() || capped_ms < 0.0 {
                self.max.as_millis() as f64
            } else {
                capped_ms
            }
        };
        // Floor at 1ms UNCONDITIONALLY so a misconfigured shrinking factor
        // (`factor_milli < 1000`), a sub-millisecond `base`/`max`, or
        // truncation of a sub-ms scaled value cannot yield a zero-delay restart
        // and a hot re-spawn loop. A sub-ms config is itself a misconfiguration;
        // honoring a 1ms minimum over it is the safe choice.
        Duration::from_millis(raw_ms.max(1.0) as u64)
    }
}

impl Default for Backoff {
    fn default() -> Self {
        // 500ms base, doubling, capped at 30s.
        Backoff {
            base: Duration::from_millis(500),
            factor_milli: 2000,
            max: Duration::from_secs(30),
        }
    }
}

/// Provider-supplied restart policy. `max_consecutive_failures` is the
/// provider's request; the effective ceiling is `min(it, HARD_FAILURE_CEILING)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartPolicy {
    /// The provider's REQUESTED ceiling. This is not the enforced value: read
    /// [`RestartPolicy::effective_ceiling`] for the value `decide` actually
    /// uses, which is capped at [`HARD_FAILURE_CEILING`] (LD36). The raw
    /// request is preserved here for display/logging.
    pub max_consecutive_failures: u32,
    pub backoff: Backoff,
}

impl RestartPolicy {
    pub fn new(max_consecutive_failures: u32, backoff: Backoff) -> Self {
        RestartPolicy {
            max_consecutive_failures,
            backoff,
        }
    }

    /// The effective ceiling after applying the LD36 hard cap.
    pub fn effective_ceiling(&self) -> u32 {
        self.max_consecutive_failures.min(HARD_FAILURE_CEILING)
    }

    /// Decide what to do after `consecutive_failures` post-spawn crashes.
    pub fn decide(&self, consecutive_failures: u32) -> RestartDecision {
        if consecutive_failures >= self.effective_ceiling() {
            RestartDecision::PermanentDead
        } else {
            RestartDecision::Restart {
                after: self.backoff.delay_for(consecutive_failures),
            }
        }
    }
}

impl Default for RestartPolicy {
    fn default() -> Self {
        // A provider that supplies nothing gets 5 retries with default backoff,
        // still under the hard ceiling of 10.
        RestartPolicy::new(5, Backoff::default())
    }
}

/// The decision the supervisor acts on after a crash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartDecision {
    /// Re-spawn the child after `after`.
    Restart { after: Duration },
    /// Hard ceiling reached; mark `permanent_dead`, never restart again.
    PermanentDead,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hard_ceiling_overrides_generous_provider_policy() {
        // Provider asks for 100 retries; LD36 caps it at 10.
        let policy = RestartPolicy::new(100, Backoff::default());
        assert_eq!(policy.effective_ceiling(), HARD_FAILURE_CEILING);
        assert_eq!(
            policy.decide(9),
            RestartDecision::Restart {
                after: policy.backoff.delay_for(9)
            }
        );
        assert_eq!(policy.decide(10), RestartDecision::PermanentDead);
        assert_eq!(policy.decide(50), RestartDecision::PermanentDead);
    }

    #[test]
    fn conservative_provider_policy_triggers_permanent_dead_early() {
        // Provider asks for only 3 retries; that is below the ceiling and wins.
        let policy = RestartPolicy::new(3, Backoff::default());
        assert_eq!(policy.effective_ceiling(), 3);
        assert!(matches!(policy.decide(2), RestartDecision::Restart { .. }));
        assert_eq!(policy.decide(3), RestartDecision::PermanentDead);
    }

    #[test]
    fn backoff_grows_and_caps() {
        let backoff = Backoff {
            base: Duration::from_millis(100),
            factor_milli: 2000, // x2
            max: Duration::from_millis(800),
        };
        assert_eq!(backoff.delay_for(1), Duration::from_millis(100));
        assert_eq!(backoff.delay_for(2), Duration::from_millis(200));
        assert_eq!(backoff.delay_for(3), Duration::from_millis(400));
        assert_eq!(backoff.delay_for(4), Duration::from_millis(800));
        // Past the cap, stays capped.
        assert_eq!(backoff.delay_for(10), Duration::from_millis(800));
    }

    #[test]
    fn shrinking_factor_floors_above_zero() {
        // A misconfigured factor < 1.0 shrinks the delay; truncation toward 0
        // would otherwise produce a zero-delay hot restart loop.
        let backoff = Backoff {
            base: Duration::from_millis(2),
            factor_milli: 100, // 0.1x: shrinks fast
            max: Duration::from_secs(30),
        };
        // By the 5th failure the scaled value is well below 1ms; must floor.
        assert!(
            backoff.delay_for(5) >= Duration::from_millis(1),
            "backoff must never floor to zero: got {:?}",
            backoff.delay_for(5)
        );
    }

    #[test]
    fn sub_millisecond_max_still_floors_above_zero() {
        // A degenerate sub-1ms max must not let the floor truncate to 0 and
        // reintroduce a hot restart loop.
        let backoff = Backoff {
            base: Duration::from_micros(100),
            factor_milli: 2000,
            max: Duration::from_micros(500), // < 1ms; as_millis() truncates to 0
        };
        assert!(
            backoff.delay_for(1) >= Duration::from_millis(1),
            "sub-ms max must still floor to >=1ms, got {:?}",
            backoff.delay_for(1)
        );
        assert!(backoff.delay_for(5) >= Duration::from_millis(1));
    }

    #[test]
    fn first_failure_uses_base_not_zero() {
        let policy = RestartPolicy::default();
        match policy.decide(1) {
            RestartDecision::Restart { after } => assert!(after >= Duration::from_millis(1)),
            other => panic!("expected restart, got {other:?}"),
        }
    }
}
