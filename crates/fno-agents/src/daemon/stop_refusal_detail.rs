//! The stop pane-row refusal text, one builder for every pane reading.
//!
//! Named by the question it answers: what `fno agents stop` tells an operator
//! about a pane-hosted row. A pane row's ONE live ref is the mux pane
//! (state.rs invariant: mux XOR worker-socket identity XOR bg thread), and
//! `stop` reaches no pane. Answering it with a success would report work this
//! verb did not perform over a live pane - the zombie shape. Refuse and name
//! the working verb with the row's own ref, in handle_rm's refusal voice.
//! Keys on the row's mux ref, never the harness, so it covers claude, codex,
//! opencode, and agy pane rows in one branch.
//!
//! x-3448: the refusal used to key on the stored mux ref alone and advise
//! `fno mux pane kill` even when the pane was already gone - the operator did
//! the named thing and the row still would not clear. When a probe proves the
//! pane absent, this builder runs the shared pane-stop precheck (the one rm
//! and the reap dry run consult) and names the command that works next, with
//! the reading that decides it. The refusal itself is unchanged: stop still
//! refuses every pane row, performs nothing, and releases no claims (ruling
//! d-658e6834).

use super::PaneProbe;
use crate::pane_stop::PanePrecheck;
use crate::state::RegistryEntry;

/// Probe the row's pane, then run the shared pane-stop precheck when the pane
/// is already gone. Each read is a blocking subprocess, so each runs through
/// `off_executor` like handle_rm_with's chain; a present pane never pays for
/// the lsof/holder read.
pub(crate) fn pane_verdict(entry: &RegistryEntry) -> (PaneProbe, Option<PanePrecheck>) {
    let Some(mux) = entry.mux.as_ref() else {
        return (PaneProbe::Unknown, None);
    };
    let probe = super::off_executor(|| super::run_mux_pane_probe(&mux.session, mux.pane_id));
    let precheck = (probe == PaneProbe::Absent).then(|| {
        let owned = entry.clone();
        super::off_executor(move || crate::pane_stop::precheck_pane_stop(&owned))
    });
    (probe, precheck)
}

/// Build the InvalidParams detail for a pane-hosted row's stop refusal.
/// `pane` is the probe verdict over the row's mux ref; `precheck` is the
/// shared pane-stop precheck answer, carried only when the probe answered
/// `Absent` (a present pane never pays for the holder read).
pub(crate) fn pane_row_refusal(
    name: &str,
    session: &str,
    pane_id: u64,
    pane: PaneProbe,
    precheck: Option<&PanePrecheck>,
) -> String {
    if pane == PaneProbe::Absent {
        if let Some(PanePrecheck::AlreadyStopped(d)) = precheck {
            // The three independent readings agree: the pid is gone, the
            // harness's own reader finds no holder. `rm` proceeds on this
            // verdict with no override, so name it as the next command.
            return format!(
                "agent {name} is a pane worker; `stop` reaches no pane. Its pane \
                 {session}:{pane_id} is already gone and the stop is proven: {d}. \
                 Clear the row with `fno agents rm {name}`."
            );
        }
        if let Some(PanePrecheck::Unprovable(d)) = precheck {
            // The pane is gone but the stop cannot be proven; rm refuses on
            // the same reading, so say that instead of sending the operator
            // into the loop this node filed.
            return format!(
                "agent {name} is a pane worker; `stop` reaches no pane. Its pane \
                 {session}:{pane_id} is already gone, but the stop cannot be \
                 proven: {d}. `fno agents rm {name}` refuses on that same \
                 reading until it changes."
            );
        }
        if let Some(PanePrecheck::NeedsKill) = precheck {
            // The pane is gone but a live pid remains: killing the (already
            // absent) pane cannot be the remedy, and rm is the verb that
            // signals the pid - only under the same proof it always applies.
            return format!(
                "agent {name} is a pane worker; `stop` reaches no pane. Its pane \
                 {session}:{pane_id} is already gone, but its pid still runs, so \
                 the stop is not proven. `fno agents rm {name}` signals that pid \
                 only when it can prove the process is this row's, and removes \
                 the row once the pid is gone."
            );
        }
    }
    // Present, Unknown, or no precheck answer: today's text byte for byte
    // (cli/tests/test_recovery.py pins the prefix), so the operator hears the
    // familiar advice whenever absence is not proven.
    format!(
        "agent {name} is a pane worker; `stop` reaches no pane and would report a \
         stop it did not perform. Kill the pane: \
         `fno mux pane kill {session}:{pane_id}`. The registry row survives that; \
         clear it with `fno agents rm {name}`."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The already-gone, proven branch: names rm, quotes the detail, never
    /// advises the pane kill (AC1-HP).
    #[test]
    fn absent_and_already_stopped_names_rm_with_the_proof() {
        let text = pane_row_refusal(
            "bp-x",
            "main",
            2065,
            PaneProbe::Absent,
            Some(&PanePrecheck::AlreadyStopped(
                "pid 536 is gone (ESRCH); the codex rollout has no live holder".into(),
            )),
        );
        assert!(
            text.contains("is already gone and the stop is proven"),
            "{text}"
        );
        assert!(
            text.contains("pid 536 is gone (ESRCH); the codex rollout has no live holder"),
            "{text}"
        );
        assert!(text.contains("`fno agents rm bp-x`"), "{text}");
        assert!(!text.contains("fno mux pane kill"), "{text}");
        assert!(!text.contains("--force"), "{text}");
    }

    /// The already-gone, unprovable branch: says rm refuses on the same
    /// reading, quotes the detail, never advises the pane kill or a force
    /// (AC1-ERR).
    #[test]
    fn absent_and_unprovable_names_rm_refusal_with_the_reason() {
        let text = pane_row_refusal(
            "bp-y",
            "main",
            10,
            PaneProbe::Absent,
            Some(&PanePrecheck::Unprovable(
                "pane row carries no verified pid; the stop cannot be proven".into(),
            )),
        );
        assert!(
            text.contains("is already gone, but the stop cannot be proven"),
            "{text}"
        );
        assert!(
            text.contains("pane row carries no verified pid; the stop cannot be proven"),
            "{text}"
        );
        assert!(
            text.contains("`fno agents rm bp-y` refuses on that same reading"),
            "{text}"
        );
        assert!(!text.contains("fno mux pane kill"), "{text}");
        assert!(!text.contains("--force"), "{text}");
    }

    /// The already-gone, live-pid branch: names rm as the signal path, never
    /// the pane kill (AC2-EDGE).
    #[test]
    fn absent_and_needs_kill_names_rm_as_the_signal_path() {
        let text = pane_row_refusal(
            "bp-z",
            "main",
            10,
            PaneProbe::Absent,
            Some(&PanePrecheck::NeedsKill),
        );
        assert!(text.contains("its pid still runs"), "{text}");
        assert!(text.contains("`fno agents rm bp-z`"), "{text}");
        assert!(!text.contains("fno mux pane kill"), "{text}");
        assert!(!text.contains("--force"), "{text}");
    }

    /// Present, Unknown, or no precheck: byte-for-byte today's refusal, the
    /// prefix cli/tests/test_recovery.py pins (AC3-HP). The pane-kill advice
    /// lives on this branch only.
    #[test]
    fn present_unknown_or_unread_matches_todays_refusal_byte_for_byte() {
        let expected = "agent w is a pane worker; `stop` reaches no pane and would \
         report a stop it did not perform. Kill the pane: `fno mux pane kill \
         main:10`. The registry row survives that; clear it with `fno agents rm w`."
            .to_string();
        for pane in [PaneProbe::Present, PaneProbe::Unknown] {
            assert_eq!(
                pane_row_refusal("w", "main", 10, pane, Some(&PanePrecheck::NeedsKill)),
                expected,
                "{pane:?} must ignore the precheck"
            );
            assert_eq!(
                pane_row_refusal("w", "main", 10, pane, None),
                expected,
                "{pane:?} must ignore a missing precheck"
            );
        }
    }
}
