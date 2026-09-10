//! The served pair's writer and planner: the served word (`liveness` +
//! `liveness_measured_at`) and the reconcile-change applier.
//!
//! Split from daemon.rs: the file is over the line budget and shrink-only,
//! so new liveness code lands here and the code it touches moves with it.

use fno::served_liveness::SERVED_LIVENESS_CADENCE;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crate::client_verbs::RowLiveness;
use crate::daemon::{is_codex_thread_entry, is_non_terminal};
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

/// A status change reconcile decided for one probed entry. `new_status: None`
/// means "probed, status unchanged" — its `last_reconciled_at` is still bumped
/// so the fairness ordering rotates.
pub(crate) struct ReconcileChange {
    pub(crate) name: String,
    pub(crate) new_status: Option<AgentStatus>,
    /// The probe's liveness word, `alive|dead|unmeasured`, decided
    /// where the evidence was gathered and written beside
    /// `liveness_measured_at`. `None` = not measured this sweep (deferred or
    /// no evidence): leave the previous measurement standing, its age honest
    /// on the wire.
    pub(crate) new_liveness: Option<&'static str>,
}

/// What a reconcile sweep did, for the `reconcile_done` event and tests.
#[derive(Default, PartialEq, Debug)]
pub(crate) struct ReconcileOutcome {
    pub(crate) updated: Vec<String>,
    pub(crate) orphans: Vec<String>,
    pub(crate) recovered: Vec<String>,
    /// `(name, reason)` for entries whose probe was inconclusive (status
    /// preserved, never flipped).
    pub(crate) inconsistent: Vec<(String, String)>,
    /// Count of trailing entries not probed because the budget elapsed.
    pub(crate) deferred: usize,
}

/// Plan a reconcile sweep over `entries` (which the caller has ordered ASC by
/// `last_reconciled_at` for fairness). Pure of clock and I/O: `probe` answers
/// reachability tri-state per entry and `budget_exhausted` reports whether the
/// sweep budget has elapsed — both injected so the budget/fairness/tri-state
/// logic is deterministically unit-testable (the daemon wires the real provider
/// probe + a wall-clock deadline).
///
/// Transition rules (status-aware, design AC9):
/// - `Ok(true)` (reachable): recover an `Orphaned` entry to `Live`; leave any
///   other status (live-ish or terminal) unchanged.
/// - `Ok(false)` (unreachable): flip a live-ish entry to `Orphaned`; leave an
///   already-`Orphaned` or terminal (`Exited`/`PermanentDead`) entry unchanged.
/// - `Err` (inconclusive): preserve status, record an inconsistency. Never
///   orphan on a probe timeout (Failure Modes / Errors invariant).
/// - Ask-bucket rows (one-shot asks AND claude bg threads, x-5d96): a roster
///   `bg_live` hit plus a SILENT liveness ladder (no socket, no advancing
///   heartbeat, no working truth state) transitions `Orphaned` - the
///   reversible state - so roster presence can no longer pin a zombie row
///   `live` forever. An `Alive` ladder answer blocks the flip.
///
/// `liveness` is the shared reader (x-5d96), injected like `probe` so the
/// ladder is deterministically stageable in tests.
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan_reconcile<P, D, L, B, H, R, V>(
    entries: &[RegistryEntry],
    mut probe: P,
    mut budget_exhausted: D,
    mut pid_live: L,
    mut bg_live: B,
    mut thread_hosted: H,
    mut rollout_exists: R,
    mut liveness: V,
    roster_readable: bool,
) -> (Vec<ReconcileChange>, ReconcileOutcome)
where
    P: FnMut(&RegistryEntry) -> Result<bool, crate::provider::ReachabilityProbeError>,
    D: FnMut() -> bool,
    L: FnMut(&RegistryEntry) -> bool,
    B: FnMut(&RegistryEntry) -> bool,
    H: FnMut(&RegistryEntry) -> bool,
    R: FnMut(&RegistryEntry) -> bool,
    V: FnMut(&RegistryEntry) -> RowLiveness,
{
    let mut changes = Vec::new();
    let mut out = ReconcileOutcome::default();
    for (i, entry) in entries.iter().enumerate() {
        if budget_exhausted() {
            out.deferred = entries.len() - i;
            break;
        }
        // A Codex thread hosted by THIS daemon is owned by its actor: the
        // stale registry pid must not settle it. A row no longer hosted is
        // settled by its rollout: the rollout file on disk is the durable
        // object, so its presence means Orphaned (resumable later, by a human
        // or a resume verb), its absence means the thread never got far enough
        // to persist anything and is Exited. Before the actor rewrite this arm
        // always returned None, so a permanently dead thread read Live forever.
        if is_codex_thread_entry(entry) {
            let hosted = thread_hosted(entry);
            let new_status = if hosted {
                None
            } else if rollout_exists(entry) {
                out.updated.push(entry.name.clone());
                Some(AgentStatus::Orphaned)
            } else {
                out.updated.push(entry.name.clone());
                Some(AgentStatus::Exited)
            };
            changes.push(ReconcileChange {
                name: entry.name.clone(),
                new_status,
                // Hosted = the actor answers for it: a positive running
                // marker, so the measurement is served fresh instead of
                // keeping a stale stored word standing. A rollout means
                // resumable, not running; nothing on disk is gone.
                new_liveness: if hosted {
                    Some("alive")
                } else {
                    match new_status {
                        Some(AgentStatus::Exited) | Some(AgentStatus::Orphaned) => Some("dead"),
                        _ => None,
                    }
                },
            });
            continue;
        }
        // A one-shot `ask` agent has no daemon-managed process, so its liveness is
        // decided by process-liveness alone (it has none): terminal `exited`.
        // Session-file reachability answers "resumable?" (surfaced via session_id),
        // never "running?" -- so a surviving session file must NOT keep an ask row
        // `live`. This is the actual cause of the reported stale-`live` rows: the
        // `probe` is skipped entirely here, so no provider reachability call can
        // decide an ask row's status. An already-terminal ask is left untouched.
        // [plan ab-70faa65b, Locked Decision #1]
        // A `claude --substrate bg` thread lands in this same bucket (claude
        // harness, no footnote pid, no mux) and yet it IS a running process --
        // claude's own daemon owns it and lists it in `roster.json`. Reaping it
        // unprobed made `wait --state done` answer "done (via exit)" seconds
        // after spawn, for a worker whose transcript was still growing, so a
        // court king read a live teammate as dead and could respawn a duplicate
        // against it. `bg_live` asks the roster before we declare death; a
        // genuinely finished ask is absent from it and still reaps to Exited.
        if entry.is_one_shot_ask() {
            // Ask the ladder once, up front: an Alive answer is a positive
            // running marker and is served as `alive` below. Behind the old
            // Unknown-only orphan test the answer was discarded for every
            // healthy row, so the served word kept a stale stored value
            // standing forever (measured: 0 of 35 claude rows read alive).
            let measured = liveness(entry);
            let new_status = if is_non_terminal(entry.status) && !bg_live(entry) {
                out.updated.push(entry.name.clone());
                Some(AgentStatus::Exited)
            } else if matches!(
                entry.status,
                AgentStatus::Live | AgentStatus::Ready | AgentStatus::Idle | AgentStatus::Busy
            ) && roster_readable
                && bg_live(entry)
                && measured == RowLiveness::Unknown
            {
                // x-5d96: a roster entry used to hold a claude row `live`
                // forever. Roster presence is weak evidence - a dead
                // supervisor can leave stale entries - so a row the shared
                // ladder answers `Unknown` on (no socket, no advancing
                // heartbeat, no working truth state) carries no positive
                // running-marker and goes `Orphaned` - never `Exited`,
                // silence never proves death. The flip needs a roster read
                // that SUCCEEDED: an unreadable roster is unknown liveness,
                // and orphaning a live worker on a transient instrumentation
                // failure is the false positive this arm must not produce.
                // Spawning is EXCLUDED: a row still coming up has had no
                // chance to produce any marker, so its silence is
                // meaningless (the same never-reap-something-still-coming-up
                // rule the sweep uses). An advancing heartbeat or a working
                // truth state answers Alive and blocks the flip (the x-d3ad
                // resurrected session). An Orphaned row is not re-visited
                // here (not live-ish), and gc still protects it: removal
                // needs positive corroboration, so a falsely-flipped live
                // worker keeps its transcript evidence and is never reaped.
                out.orphans.push(entry.name.clone());
                out.updated.push(entry.name.clone());
                Some(AgentStatus::Orphaned)
            } else {
                None
            };
            changes.push(ReconcileChange {
                name: entry.name.clone(),
                new_status,
                // The ask arm's evidence, not a guess: a bg-live roster hit
                // with a silent ladder never positively answers, so it reads
                // unmeasured, never dead; a finished ask is gone.
                new_liveness: if measured == RowLiveness::Alive {
                    Some("alive")
                } else {
                    match new_status {
                        Some(AgentStatus::Exited) => Some("dead"),
                        Some(AgentStatus::Orphaned) => Some("unmeasured"),
                        _ => None,
                    }
                },
            });
            continue;
        }
        // One probe, two verdicts: the status transition (below) and the
        // SERVED liveness word both come from the same measurement,
        // so the wire can never claim an age or a word the sweep did not
        // itself just observe.
        let measured = probe(entry);
        let new_status = match &measured {
            Ok(true) => {
                // Recovery needs BOTH signals. A store hit alone means "the
                // session still exists" (= resumable), which for a store that
                // never evicts is permanently true - opencode's session table
                // keeps a row forever, so a dead pane would be resurrected to
                // `live` on every sweep and discovery would hand out a
                // recipient nobody drains. A row with no recorded pid keeps the
                // old behavior (`pid_live` is true), so exec rows are untouched.
                // Ask-bucket rows never reach this arm (they continue above),
                // so an Orphaned x-5d96 zombie cannot recover here and
                // oscillate: gc ages it from the terminal set instead.
                if entry.status == AgentStatus::Orphaned && pid_live(entry) {
                    out.recovered.push(entry.name.clone());
                    out.updated.push(entry.name.clone());
                    Some(AgentStatus::Live)
                } else {
                    None
                }
            }
            Ok(false) if entry.is_interactive() => {
                // host_mode=interactive (task 2.3 / US4): a daemon-managed
                // interactive host is always pid'd; its liveness is the PTY
                // process, not the session store, so a store miss must not orphan
                // it. A dead worker reaps to Exited ("unexpected exit is exited,
                // not orphaned"; Codex P2, PR #373).
                if pid_live(entry) {
                    None
                } else {
                    out.updated.push(entry.name.clone());
                    Some(AgentStatus::Exited)
                }
            }
            Ok(false) if entry.mux.is_some() => {
                // A mux-pane row is PTY-governed only with a captured pid. Mux
                // rows are written with the default exec host_mode but carry a mux
                // ref; without this arm, 1.1's backfilled codex id (or a claude
                // pane's minted id) would false-orphan a live pane on a store
                // miss. But pid_live maps None to true, so a pid-less mux row
                // (_lookup_child_pid best-effort miss) must NOT be preserved here
                // or a maybe-dead pane stays immortal -- it defers to store
                // liveness (orphan) instead. A live pid keeps it Live; a dead pid
                // reaps to Exited (Codex P1/P2, #603 r3/r4).
                if entry.pid.is_some() && pid_live(entry) {
                    None
                } else if entry.pid.is_some() {
                    out.updated.push(entry.name.clone());
                    Some(AgentStatus::Exited)
                } else {
                    let live_ish = matches!(
                        entry.status,
                        AgentStatus::Live
                            | AgentStatus::Ready
                            | AgentStatus::Idle
                            | AgentStatus::Busy
                            | AgentStatus::Spawning
                    );
                    if live_ish {
                        out.orphans.push(entry.name.clone());
                        out.updated.push(entry.name.clone());
                        Some(AgentStatus::Orphaned)
                    } else {
                        None
                    }
                }
            }
            Ok(false) => {
                // Only states that *should* have a live backend can go stale.
                // Restarting / Failed are intentionally excluded: the restart
                // supervisor owns those agents' lifecycle (backoff -> re-spawn
                // or permanent_dead), so reconcile must not race it by flipping
                // a mid-restart agent to orphaned. Terminal states (Exited /
                // PermanentDead) are likewise left alone.
                let live_ish = matches!(
                    entry.status,
                    AgentStatus::Live
                        | AgentStatus::Ready
                        | AgentStatus::Idle
                        | AgentStatus::Busy
                        | AgentStatus::Spawning
                );
                if live_ish {
                    out.orphans.push(entry.name.clone());
                    out.updated.push(entry.name.clone());
                    Some(AgentStatus::Orphaned)
                } else {
                    None
                }
            }
            Err(e) => {
                out.inconsistent
                    .push((entry.name.clone(), e.reason.clone()));
                None
            }
        };
        changes.push(ReconcileChange {
            name: entry.name.clone(),
            new_status,
            new_liveness: crate::liveness_sweep::served_word(entry, &measured, &mut pid_live),
        });
    }
    (changes, out)
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

/// Which writes a sweep applies. [`SweepMode::Full`] is the startup pass and
/// the `agent.reconcile` RPC, exactly as before. [`SweepMode::ServeOnly`] is
/// the daemon's 60s liveness tick: the SAME measurement and the SAME served
/// word a full sweep would serve, but no lifecycle write - the orphan flip,
/// the exit reap, and their stamps stay where they are today, on the
/// operator-driven sweeps.
pub(crate) enum SweepMode {
    Full,
    ServeOnly,
}

/// The one batched registry write both modes share: apply every planned
/// change and the batch's title readings in one lock window. ServeOnly
/// passes `None` for every status, so a row the plan would move to Exited
/// or Orphaned keeps its status, pid, and exited_at, and only the served
/// pair and the CHECKED stamp advance.
pub(crate) fn apply_reconcile_changes(
    r: &mut state::Registry,
    entries: &[RegistryEntry],
    changes: &[crate::daemon::ReconcileChange],
    titles: &std::collections::HashMap<String, Option<String>>,
    mode: &SweepMode,
    now: &str,
) {
    for ch in changes {
        // Keyed on the probed row's identity read off the same snapshot the
        // sweep planned from, so a row replaced under the same label between
        // snapshot and locked write cannot receive the first row's status.
        let ident = entries
            .iter()
            .find(|e| e.name == ch.name)
            .map(state::registry_write_key);
        let keyed = ident
            .as_ref()
            .and_then(|(h, sid)| sid.as_deref().and_then(|sid| r.find_by_session_mut(h, sid)));
        let target = match keyed {
            Some(e) => Some(e),
            None => r.find_mut(&ch.name),
        };
        if let Some(e) = target {
            let status = match mode {
                SweepMode::Full => ch.new_status,
                SweepMode::ServeOnly => None,
            };
            apply_reconcile_change(e, status, ch.new_liveness, now);
        }
    }
    // Apply the batch's title readings in the SAME lock window: the row's
    // stored title is the diff baseline the next sweep compares against, so
    // a row the reconcile changes never skipped lost its rename.
    crate::row_truth::apply_title_changes(r, entries, titles);
}

/// One tick's gate decision: due only past the cadence AND with the previous
/// sweep's slot free. `swap(true)` claims the slot for this tick.
fn due(last_sweep: Instant, in_flight: &AtomicBool) -> bool {
    last_sweep.elapsed() >= SERVED_LIVENESS_CADENCE && !in_flight.swap(true, Ordering::SeqCst)
}

/// The daemon tick's whole serve-only liveness arm: throttled to
/// [`SERVED_LIVENESS_CADENCE`], one-in-flight behind `in_flight`, run off
/// the accept loop on the blocking pool (never inline in a select arm: an
/// in-arm sweep against a wedged read is the unreachable-AND-unstoppable
/// shape that loop's rule exists to prevent). Same shape as
/// `orphan_reap::maybe_sweep`.
pub(crate) fn maybe_sweep(
    last_sweep: &mut Instant,
    in_flight: &Arc<AtomicBool>,
    home: crate::paths::AgentsHome,
    events: std::path::PathBuf,
    thread_hosted: Arc<dyn Fn(&RegistryEntry) -> bool + Send + Sync>,
) {
    if !due(*last_sweep, in_flight) {
        return;
    }
    *last_sweep = Instant::now();
    let flag = Arc::clone(in_flight);
    tokio::task::spawn_blocking(move || {
        let _gate = crate::daemon::SweepGate(flag);
        let emitter = crate::events::EventEmitter::new(events, "daemon");
        let _ = crate::daemon::run_reconcile_sweep(
            &home,
            &emitter,
            thread_hosted.as_ref(),
            SweepMode::ServeOnly,
        );
    });
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

    #[test]
    fn the_tick_gate_fires_once_per_cadence_and_once_per_slot() {
        // AC2-EDGE at the gate: a fresh stamp is not due; an aged one is due
        // exactly once (the slot is claimed by the first call), and a second
        // tick inside the flight is a no-op.
        let in_flight = Arc::new(AtomicBool::new(false));
        let fresh = Instant::now();
        assert!(!due(fresh, &in_flight), "inside the cadence: not due");
        assert!(
            !in_flight.load(Ordering::SeqCst),
            "the slot was never claimed"
        );

        let aged = Instant::now() - SERVED_LIVENESS_CADENCE;
        assert!(
            due(aged, &in_flight),
            "past the cadence with a free slot: due"
        );
        assert!(in_flight.load(Ordering::SeqCst), "the slot is claimed");
        let aged_again = Instant::now() - SERVED_LIVENESS_CADENCE;
        assert!(!due(aged_again, &in_flight), "a sweep in flight: not due");
    }

    #[test]
    fn serve_only_apply_keeps_lifecycle_state_and_freshens_the_stamp() {
        // AC2-ERR: a row the plan WOULD move to Exited keeps its status,
        // pid, and exited_at under the serve-only tick; only the served
        // pair and the CHECKED stamp advance. The full mode stays the
        // lifecycle writer.
        let mk = |name: &str, pid: Option<u32>| {
            let mut e = state::RegistryEntry::default();
            e.name = name.to_string();
            e.status = AgentStatus::Live;
            e.mux = Some(crate::state::MuxRef {
                session: "main".into(),
                pane_id: 7,
            });
            e.pid = pid;
            e
        };
        let entries = vec![mk("planned-exit", Some(4242))];
        let changes = vec![crate::daemon::ReconcileChange {
            name: "planned-exit".into(),
            new_status: Some(AgentStatus::Exited),
            new_liveness: Some("dead"),
        }];
        let titles: std::collections::HashMap<String, Option<String>> =
            std::collections::HashMap::new();
        let mut reg = state::Registry::default();
        reg.entries = entries.clone();

        crate::liveness_sweep::apply_reconcile_changes(
            &mut reg,
            &entries,
            &changes,
            &titles,
            &SweepMode::ServeOnly,
            "2026-09-10T12:00:00Z",
        );
        let row = reg.find_mut("planned-exit").unwrap();
        assert_eq!(
            row.status,
            AgentStatus::Live,
            "serve-only never flips status"
        );
        assert_eq!(row.pid, Some(4242), "serve-only keeps the pid");
        assert_eq!(row.exited_at, None, "serve-only stamps no exit");
        assert_eq!(row.liveness.as_deref(), Some("dead"), "the word IS written");
        assert_eq!(
            row.liveness_measured_at.as_deref(),
            Some("2026-09-10T12:00:00Z"),
            "the stamp IS written"
        );

        // The full mode is still the lifecycle writer: the same change
        // applied Full reaps the row and clears its pid.
        let mut full_reg = state::Registry::default();
        full_reg.entries = entries.clone();
        crate::liveness_sweep::apply_reconcile_changes(
            &mut full_reg,
            &entries,
            &changes,
            &titles,
            &SweepMode::Full,
            "2026-09-10T12:00:00Z",
        );
        let row = full_reg.find_mut("planned-exit").unwrap();
        assert_eq!(row.status, AgentStatus::Exited);
        assert_eq!(row.pid, None, "Exited clears the pid (Locked Decision #7)");
        assert_eq!(row.exited_at.as_deref(), Some("2026-09-10T12:00:00Z"));
    }
}
