//! The one served-liveness freshness rule both readers call.
//!
//! `fno agents list` (crates/fno-agents) and the mux roster reader
//! ([`crate::agents_view`]) both answer "is this row live NOW" from the
//! reconcile sweep's stored measurement. The rule lives here once; a reader
//! that restates it drifts, which is how the two windows disagreed before.

use std::time::Duration;

use crate::agents_view::Liveness;

/// How often the daemon's serve-only tick re-measures the served pair.
/// The sweep's shared reads (the batched truth probe alone is near a
/// second cold) are too heavy to run continuously, so the tick runs on
/// this cadence and the freshness window tolerates one missed tick.
pub const SERVED_LIVENESS_CADENCE: Duration = Duration::from_secs(60);

/// A measurement older than twice the cadence no longer answers "is this
/// row alive NOW": one missed tick still serves, two do not. Republishing
/// an old word is what made a 24-hour-old `dead` read as current.
pub const SERVED_LIVENESS_MAX_AGE_SECS: u64 = 2 * 60;

fn fresh(measured_at_secs: Option<u64>, now_secs: u64) -> bool {
    measured_at_secs
        .and_then(|stamp| now_secs.checked_sub(stamp))
        .is_some_and(|age| age <= SERVED_LIVENESS_MAX_AGE_SECS)
}

/// The stored liveness word, served only while its stamp is fresh. An
/// absent/stale measurement or an unknown word answers `None` and the
/// caller's status ladder takes over exactly as before.
pub fn served_liveness(
    word: Option<&str>,
    measured_at_secs: Option<u64>,
    now_secs: u64,
) -> Option<Liveness> {
    if !fresh(measured_at_secs, now_secs) {
        return None;
    }
    match word? {
        "alive" => Some(Liveness::Alive),
        "dead" => Some(Liveness::Dead),
        "unmeasured" => Some(Liveness::Unmeasured),
        _ => None,
    }
}

/// The same gate at the word level, for a reader that passes the stored
/// string through verbatim (the daemon's list projection).
pub fn served_liveness_word<'a>(
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

/// Why the row's `liveness` reads the way it does: `fresh` inside the
/// window, `stale` once a word and stamp exist but the window has passed
/// (the word is withheld), `never-measured` with no word or no stamp. A
/// served field carries its basis; a null with no reason beside it
/// answers nothing.
pub fn served_liveness_basis(
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
    fn a_90_second_old_word_still_serves() {
        assert_eq!(
            served_liveness(Some("alive"), Some(NOW - 90), NOW),
            Some(Liveness::Alive)
        );
    }

    #[test]
    fn a_word_past_the_window_is_withheld() {
        assert_eq!(served_liveness(Some("alive"), Some(NOW - 121), NOW), None);
        assert_eq!(
            served_liveness_word(Some("alive"), Some(NOW - 121), NOW),
            None
        );
    }

    #[test]
    fn the_basis_names_why_the_word_reads_the_way_it_does() {
        assert_eq!(
            served_liveness_basis(Some("alive"), Some(NOW - 90), NOW),
            "fresh"
        );
        assert_eq!(
            served_liveness_basis(Some("alive"), Some(NOW - 121), NOW),
            "stale"
        );
        assert_eq!(served_liveness_basis(None, None, NOW), "never-measured");
        assert_eq!(
            served_liveness_basis(Some("alive"), None, NOW),
            "never-measured"
        );
    }
}
