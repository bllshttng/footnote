//! The served-liveness freshness rule, vendored from
//! `crates/fno/src/served_liveness.rs`.
//!
//! Both crates publish separately, and the fno release carrying the module
//! is not on crates.io yet, so a real dependency stays blocked on the
//! publish gate. This mirror holds the rule until a synchronized 0.3.x
//! release lets fno-agents depend on the published crate; the contract
//! parity test below makes silent drift impossible, and its deletion is
//! the trigger to retire this file.

use std::time::Duration;

/// How often the daemon's serve-only tick re-measures the served pair.
/// The sweep's shared reads (the batched truth probe alone is near a
/// second cold) are too heavy to run continuously, so the tick runs on
/// this cadence and the freshness window tolerates one missed tick.
pub(crate) const SERVED_LIVENESS_CADENCE: Duration = Duration::from_secs(60);

/// A measurement older than twice the cadence no longer answers "is this
/// row alive NOW": one missed tick still serves, two do not. Republishing
/// an old word is what made a 24-hour-old `dead` read as current.
pub(crate) const SERVED_LIVENESS_MAX_AGE_SECS: u64 = 2 * 60;

pub(crate) fn fresh(measured_at_secs: Option<u64>, now_secs: u64) -> bool {
    measured_at_secs
        .and_then(|stamp| now_secs.checked_sub(stamp))
        .is_some_and(|age| age <= SERVED_LIVENESS_MAX_AGE_SECS)
}

/// The stored liveness word, served only while its stamp is fresh. An
/// absent/stale measurement answers `None` and the reader falls back to
/// the status ladder exactly as before.
pub(crate) fn served_liveness_word<'a>(
    word: Option<&'a str>,
    measured_at_secs: Option<u64>,
    now_secs: u64,
) -> Option<&'a str> {
    if fresh(measured_at_secs, now_secs) {
        word
    } else {
        None
    }
}

/// Why the row's served `liveness` reads the way it does: `fresh` inside
/// the window, `stale` once a word and stamp exist but the window has
/// passed (the word is withheld), `never-measured` with no word or no
/// stamp. A served field carries its basis; a null with no reason beside
/// it answers nothing.
pub(crate) fn served_liveness_basis(
    word: Option<&str>,
    measured_at_secs: Option<u64>,
    now_secs: u64,
) -> &'static str {
    match (word, measured_at_secs) {
        (Some(_), Some(_)) if fresh(measured_at_secs, now_secs) => "fresh",
        (Some(_), Some(_)) => "stale",
        _ => "never-measured",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    #[test]
    fn the_vendored_rule_matches_the_fno_crate_module_by_contract() {
        // The parity guard that lets this mirror exist: the constants and
        // the observable rule must match the fno crate's module exactly.
        // Dev-only link (see Cargo.toml); when the real dependency lands,
        // delete this file and read the rule from fno directly.
        assert_eq!(
            SERVED_LIVENESS_CADENCE,
            fno::served_liveness::SERVED_LIVENESS_CADENCE
        );
        assert_eq!(
            SERVED_LIVENESS_MAX_AGE_SECS,
            fno::served_liveness::SERVED_LIVENESS_MAX_AGE_SECS
        );
        let word = Some("alive");
        for measured in [Some(NOW - 90), Some(NOW - 121), None] {
            assert_eq!(
                served_liveness_word(word, measured, NOW),
                fno::served_liveness::served_liveness_word(word, measured, NOW),
                "word gate disagrees at {measured:?}"
            );
            assert_eq!(
                served_liveness_basis(word, measured, NOW),
                fno::served_liveness::served_liveness_basis(word, measured, NOW),
                "basis disagrees at {measured:?}"
            );
        }
    }
}
