//! The served pair's writer and planner: the served word (`liveness` +
//! `liveness_measured_at`) and the reconcile-change applier.
//!
//! Split from daemon.rs: the file is over the line budget and shrink-only,
//! so new liveness code lands here and the code it touches moves with it.

use crate::provider::ReachabilityProbeError;
use crate::state::{self, RegistryEntry};
use crate::AgentStatus;

/// The served liveness word for one probed row. A pane row (mux ref or
/// interactive host) with a recorded pid is PTY-governed: its served word is
/// the pid, whatever the session-store probe said - including an `Err`
/// probe, which is a claude pane row's NORMAL reading ("no session id in
/// entry") and not an unmeasured row. A live pane and a dead pane both
/// served `unmeasured` under the old Err mapping, which is the instrument
/// outage this module exists to retire. Every other row keeps the probe
/// mapping.
pub(crate) fn served_word(
    entry: &RegistryEntry,
    measured: &Result<bool, ReachabilityProbeError>,
    pid_live: &mut dyn FnMut(&RegistryEntry) -> bool,
) -> Option<&'static str> {
    if entry.pid.is_some() && (entry.mux.is_some() || entry.is_interactive()) {
        return Some(if pid_live(entry) { "alive" } else { "dead" });
    }
    match measured {
        Ok(true) => Some("alive"),
        Ok(false) => Some("dead"),
        Err(_) => Some("unmeasured"),
    }
}

/// Apply one planned reconcile change to its registry row. Always freshens
/// `last_reconciled_at` (the probe was *attempted*, so `CHECKED` rotates even on
/// an inconclusive/no-change probe). On a status change, sets the new status and
/// -- when it is terminal `Exited` -- nulls `pid`/`pid_start_time` so `list`/
/// `--json` never surfaces a pid that no longer belongs to the agent (Locked
/// Decision #7: a stale pid is exactly the misleading liveness signal this work
/// removes; forensics live in the event log, not a dangling registry pid). The
/// pid is cleared only on `Exited` (the lone terminal status reconcile produces)
/// -- an `Orphaned` row keeps its pid, which is still the live-but-unowned
/// process an operator may want to `ps`/signal while investigating the orphan.
/// The `Exited` transition also stamps `exited_at`: `last_reconciled_at` rotates
/// on every probe, so it is a CHECKED stamp, not a transition stamp, and the only
/// timestamp a reader can attribute to the exit itself is one written here.
pub(crate) fn apply_reconcile_change(
    e: &mut RegistryEntry,
    new_status: Option<AgentStatus>,
    new_liveness: Option<&str>,
    now: &str,
) {
    e.last_reconciled_at = Some(now.to_string());
    if let Some(word) = new_liveness {
        // The sweep is the ONLY writer of the served pair: a probe
        // answer is a fact about the moment it measured, so it carries its
        // stamp with it.
        e.liveness = Some(word.to_string());
        e.liveness_measured_at = Some(now.to_string());
    }
    if let Some(s) = new_status {
        e.status = s;
        if matches!(s, AgentStatus::Exited) {
            e.pid = None;
            e.pid_start_time = None;
            e.exited_at = Some(now.to_string());
            // Ordered exit teardown (E3.3, AC-X2-4): clear the inside-leg
            // authority on exit so a stale `working` never wins after the pane
            // is gone. The completion event is published by the caller BEFORE
            // this write (publish completion -> clear authority). A scraped
            // verdict dies with the pane for the same reason.
            e.inside_leg = None;
            e.screen_state = None;
        }
        if matches!(s, AgentStatus::Orphaned) {
            // x-5d96 (codex P2, PR 1329): the transition just re-decided the
            // row's liveness from current evidence, so any `exited_at` it
            // carried is a stamp from an earlier, falsified reading. Keeping
            // it would let gc age the row on a clock that started before the
            // re-decision and skip the grace window at its first real
            // dead-observation. Cleared, gc stamps fresh.
            e.exited_at = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane_entry(name: &str, pid: Option<u32>) -> RegistryEntry {
        let mut e = state::RegistryEntry::default();
        e.name = name.to_string();
        e.mux = Some(crate::state::MuxRef {
            session: "main".into(),
            pane_id: 7,
        });
        e.pid = pid;
        e
    }

    #[test]
    fn served_word_follows_the_pid_on_pane_rows() {
        let mut pids = |e: &RegistryEntry| e.pid == Some(4242);
        let err = || {
            Err(ReachabilityProbeError::new(
                "claude",
                "no session id in entry",
            ))
        };
        assert_eq!(
            served_word(&pane_entry("live", Some(4242)), &err(), &mut pids),
            Some("alive")
        );
        assert_eq!(
            served_word(&pane_entry("dead", Some(4243)), &err(), &mut pids),
            Some("dead")
        );
        assert_eq!(
            served_word(&pane_entry("pidless", None), &err(), &mut pids),
            Some("unmeasured"),
            "pid_live maps None to true, but a pid-less pane is NOT pid-governed"
        );
    }

    #[test]
    fn served_word_keeps_the_probe_mapping_off_pane_rows() {
        let mut pids = |_: &RegistryEntry| true;
        let mut e = state::RegistryEntry::default();
        e.name = "bg".into();
        let ok_true: Result<bool, ReachabilityProbeError> = Ok(true);
        let ok_false: Result<bool, ReachabilityProbeError> = Ok(false);
        let err: Result<bool, ReachabilityProbeError> =
            Err(ReachabilityProbeError::new("claude", "store unavailable"));
        assert_eq!(served_word(&e, &ok_true, &mut pids), Some("alive"));
        assert_eq!(served_word(&e, &ok_false, &mut pids), Some("dead"));
        assert_eq!(served_word(&e, &err, &mut pids), Some("unmeasured"));
    }
}
