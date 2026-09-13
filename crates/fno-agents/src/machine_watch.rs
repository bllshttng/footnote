//! The machine_watch arm: one watcher reads the box, bands it, and escalates
//! on its own.
//!
//! Python decides (x-d6ad LD3): `machine_pressure` in doctor_footprint.py
//! computes the verdict and the reason sentence. This arm reads
//! `machine.verdict`/`machine.reason` verbatim from the same
//! `fno doctor footprint --json --cause-only` payload the spawn gate already
//! shells, and computes no machine verdict of its own. The band sits on
//! whole-machine CPU; load and the runnable count ride as context and never
//! raise a hot verdict (LD2). The arm escalates, it never gates (LD1): the
//! spawn gate keeps deciding admission.

use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::paths::AgentsHome;

/// Consecutive hot samples before a notice, and consecutive hot samples kept
/// while the throttle holds. The `under_streak` shape from spawn_gate.rs
/// (`CPU_ADMIT_SAMPLES`, x-7783 LD4): a band on a 300-second beat with no
/// debounce is a pager that cries on a 60-second spike (x-d6ad LD6).
pub const MACHINE_HOT_SAMPLES: u32 = 2;

/// The arm's own beat, matching its `KNOWN_ARMS` row (AC8).
pub const MACHINE_WATCH_INTERVAL_S: u64 = 300;

/// The arm's debounce and throttle memory, owned by the daemon loop and
/// mutated only inside the arm's one-in-flight body.
#[derive(Default)]
pub struct MachineWatchState {
    hot_streak: u32,
    calm_streak: u32,
    last_notified: Option<Instant>,
}

/// The arm as the daemon holds it: cadence stamp, one-in-flight gate, and the
/// streak/throttle memory in one handle, so the loop declares one name.
pub struct Arm {
    last_tick: Mutex<Option<Instant>>,
    in_flight: Arc<AtomicBool>,
    state: Arc<Mutex<MachineWatchState>>,
}

impl Default for Arm {
    fn default() -> Self {
        Self {
            last_tick: Mutex::new(None),
            in_flight: Arc::new(AtomicBool::new(false)),
            state: Arc::new(Mutex::new(MachineWatchState::default())),
        }
    }
}

/// One tick's outcome: the tick row's `acted`, `skip_reason` and `detail`.
pub struct WatchOutcome {
    pub acted: u64,
    pub skip_reason: Option<String>,
    pub detail: String,
}

/// One pass of the arm body, with the payload handed in and the notice send
/// handed in - the same seam `notify_signal_via` proved. `now` is the clock,
/// so tests drive the throttle without sleeping. The Python decider's verdict
/// names the branch; nothing here reads a busy fraction (AC11).
pub fn tick_machine_watch(
    state: &mut MachineWatchState,
    reading: Result<&crate::spawn_gate::FootprintCausePayload, &str>,
    mut notify: impl FnMut(&str, &str) -> bool,
    now: Instant,
) -> WatchOutcome {
    let payload = match reading {
        Ok(payload) => payload,
        Err(why) => {
            return WatchOutcome {
                acted: 0,
                skip_reason: Some("machine_unreadable".to_string()),
                detail: short(&format!("probe: {why}")),
            };
        }
    };
    let Some(machine) = payload.machine.as_ref() else {
        return WatchOutcome {
            acted: 0,
            skip_reason: Some("machine_unreadable".to_string()),
            detail: "payload carries no machine object".to_string(),
        };
    };
    match machine.verdict.as_str() {
        "calm" => {
            state.calm_streak = state.calm_streak.saturating_add(1);
            state.hot_streak = 0;
            WatchOutcome {
                acted: 0,
                skip_reason: Some("calm".to_string()),
                detail: short(&machine.reason),
            }
        }
        "hot" => {
            state.hot_streak = state.hot_streak.saturating_add(1);
            state.calm_streak = 0;
            if state.hot_streak < MACHINE_HOT_SAMPLES {
                return WatchOutcome {
                    acted: 0,
                    skip_reason: Some("debouncing".to_string()),
                    detail: short(&format!(
                        "{}/{} hot: {}",
                        state.hot_streak, MACHINE_HOT_SAMPLES, machine.reason
                    )),
                };
            }
            // Hot past the debounce. The throttle suppresses repeats while the
            // state stays hot (LD6); a state change to calm never notifies.
            if let Some(last) = state.last_notified {
                let floor = Duration::from_secs(machine.throttle_minutes.saturating_mul(60));
                if let Some(held) = now.checked_duration_since(last) {
                    if held < floor {
                        let remaining = (floor - held).as_secs();
                        return WatchOutcome {
                            acted: 0,
                            skip_reason: Some("throttled".to_string()),
                            detail: short(&format!(
                                "hot, notice held {remaining}s more: {}",
                                machine.reason
                            )),
                        };
                    }
                }
            }
            let body = notice_body(machine, payload);
            if notify("machine_watch: box hot", &body) {
                state.last_notified = Some(now);
                WatchOutcome {
                    acted: 1,
                    skip_reason: None,
                    detail: short(&format!("notified: {}", machine.reason)),
                }
            } else {
                // No state for a notice that never left: the next beat retries.
                WatchOutcome {
                    acted: 0,
                    skip_reason: Some("notify_failed".to_string()),
                    detail: short(&machine.reason),
                }
            }
        }
        // The Python decider's own "unreadable", or any word this reader does
        // not know: an unreadable sensor never reads as calm (AC3).
        _ => WatchOutcome {
            acted: 0,
            skip_reason: Some("machine_unreadable".to_string()),
            detail: short(&machine.reason),
        },
    }
}

/// The escalation body: the decider's reason sentence verbatim (busy
/// fraction, band, load_15m, runnable count, process count - AC4), then the
/// top three consumers by their own argv strings, so a keeper running from a
/// worktree target directory appears by its own path (AC7).
fn notice_body(
    machine: &crate::spawn_gate::MachinePressurePayload,
    payload: &crate::spawn_gate::FootprintCausePayload,
) -> String {
    let mut body = String::new();
    body.push_str(&machine.reason);
    let mut parts: Vec<String> = Vec::new();
    for consumer in payload.top.iter().take(3) {
        parts.push(format!(
            "{} ({:.1}%)",
            consumer.command, consumer.cpu_percent
        ));
    }
    if !parts.is_empty() {
        body.push_str(" top: ");
        body.push_str(&parts.join(", "));
    }
    body
}

/// The tick row's detail is a short human string, capped like the other arms.
fn short(text: &str) -> String {
    text.chars().take(200).collect()
}

/// The daemon-facing wrapper: due-check plus one-in-flight gate, the
/// `maybe_retirement_sweep` shape. The probe shells out (8s budget), so the
/// whole body runs off-loop; every path ends in exactly one tick row.
pub fn maybe_tick(arm: &Arm, home: AgentsHome) {
    let interval = Duration::from_secs(MACHINE_WATCH_INTERVAL_S);
    {
        let mut last = arm.last_tick.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|t| t.elapsed() < interval)
            || arm.in_flight.swap(true, Ordering::SeqCst)
        {
            return;
        }
        *last = Some(Instant::now());
    }
    let flag = Arc::clone(&arm.in_flight);
    let state = Arc::clone(&arm.state);
    tokio::task::spawn_blocking(move || {
        let _gate = crate::daemon::SweepGate(flag);
        let reading: Result<crate::spawn_gate::FootprintCausePayload, String> =
            crate::spawn_gate::footprint_cause_raw().and_then(|raw| {
                serde_json::from_str(&raw)
                    .map_err(|e| format!("footprint payload unparseable: {e}"))
            });
        let outcome = {
            let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
            tick_machine_watch(
                &mut guard,
                reading.as_ref().map_err(|s| s.as_str()),
                |title, body| crate::operator_notice::notify_operator(title, body, None),
                Instant::now(),
            )
        };
        let journal = crate::loop_runtime::Journal::new_raw(
            home.events_jsonl(),
            crate::daemon::global_events_path(&home),
        );
        crate::tick_ledger::emit_tick(
            &journal,
            "machine_watch",
            crate::tick_ledger::SCHED_DAEMON,
            outcome.acted,
            outcome.skip_reason.as_deref(),
            Some(&outcome.detail),
            interval.as_secs(),
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spawn_gate::{MachinePressurePayload, TopConsumer};

    fn machine(verdict: &str, throttle_minutes: u64) -> MachinePressurePayload {
        MachinePressurePayload {
            verdict: verdict.to_string(),
            reason: format!("machine 91.7% crosses band 90% -> {verdict}; context"),
            busy_fraction: Some(0.917),
            band: 0.9,
            machine_cores: Some(11.0),
            capacity_cores: 12.0,
            runnable: Some(160),
            processes: Some(1100),
            load_15m: Some(279.12),
            throttle_minutes,
        }
    }

    fn payload(
        machine: Option<MachinePressurePayload>,
    ) -> crate::spawn_gate::FootprintCausePayload {
        crate::spawn_gate::FootprintCausePayload::from_parts(machine, Vec::new())
    }

    fn counter(calls: &mut Vec<String>) -> impl FnMut(&str, &str) -> bool + '_ {
        move |title, body| {
            calls.push(format!("{title}|{body}"));
            true
        }
    }

    #[test]
    fn two_hot_samples_notify_exactly_once() {
        let mut state = MachineWatchState::default();
        let mut calls: Vec<String> = Vec::new();
        let now = Instant::now();
        let first = tick_machine_watch(
            &mut state,
            Ok(&payload(Some(machine("hot", 30)))),
            counter(&mut calls),
            now,
        );
        assert_eq!(first.skip_reason.as_deref(), Some("debouncing"));
        assert_eq!(first.acted, 0);
        let second = tick_machine_watch(
            &mut state,
            Ok(&payload(Some(machine("hot", 30)))),
            counter(&mut calls),
            now,
        );
        assert_eq!(second.skip_reason, None);
        assert_eq!(second.acted, 1);
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn hot_then_calm_never_notifies_and_resets_the_streak() {
        let mut state = MachineWatchState::default();
        let mut calls: Vec<String> = Vec::new();
        let now = Instant::now();
        let hot = tick_machine_watch(
            &mut state,
            Ok(&payload(Some(machine("hot", 30)))),
            counter(&mut calls),
            now,
        );
        assert_eq!(hot.skip_reason.as_deref(), Some("debouncing"));
        let calm = tick_machine_watch(
            &mut state,
            Ok(&payload(Some(machine("calm", 30)))),
            counter(&mut calls),
            now,
        );
        assert_eq!(calm.skip_reason.as_deref(), Some("calm"));
        assert_eq!(calm.acted, 0);
        assert_eq!(calls.len(), 0);
        assert_eq!(state.hot_streak, 0);
        assert_eq!(state.calm_streak, 1);
    }

    #[test]
    fn a_hot_repeat_inside_the_throttle_is_held_with_seconds_named() {
        let mut state = MachineWatchState::default();
        let mut calls: Vec<String> = Vec::new();
        let now = Instant::now();
        tick_machine_watch(
            &mut state,
            Ok(&payload(Some(machine("hot", 30)))),
            counter(&mut calls),
            now,
        );
        tick_machine_watch(
            &mut state,
            Ok(&payload(Some(machine("hot", 30)))),
            counter(&mut calls),
            now,
        );
        assert_eq!(calls.len(), 1);
        let five_minutes_later = now + Duration::from_secs(300);
        let repeat = tick_machine_watch(
            &mut state,
            Ok(&payload(Some(machine("hot", 30)))),
            counter(&mut calls),
            five_minutes_later,
        );
        assert_eq!(repeat.skip_reason.as_deref(), Some("throttled"));
        assert_eq!(repeat.acted, 0);
        assert!(repeat.detail.contains("1500s more"), "{}", repeat.detail);
        assert_eq!(calls.len(), 1, "no second notice");
    }

    #[test]
    fn a_hot_repeat_past_the_throttle_notifies_again() {
        let mut state = MachineWatchState::default();
        let mut calls: Vec<String> = Vec::new();
        let now = Instant::now();
        tick_machine_watch(
            &mut state,
            Ok(&payload(Some(machine("hot", 30)))),
            counter(&mut calls),
            now,
        );
        tick_machine_watch(
            &mut state,
            Ok(&payload(Some(machine("hot", 30)))),
            counter(&mut calls),
            now,
        );
        let past = now + Duration::from_secs(31 * 60);
        let repeat = tick_machine_watch(
            &mut state,
            Ok(&payload(Some(machine("hot", 30)))),
            counter(&mut calls),
            past,
        );
        assert_eq!(repeat.skip_reason, None);
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn an_unreadable_sensor_notifies_nobody() {
        let mut state = MachineWatchState::default();
        let mut calls: Vec<String> = Vec::new();
        let now = Instant::now();
        let probe_failed = tick_machine_watch(
            &mut state,
            Err("footprint probe did not answer inside 8s"),
            counter(&mut calls),
            now,
        );
        assert_eq!(
            probe_failed.skip_reason.as_deref(),
            Some("machine_unreadable")
        );
        assert!(
            probe_failed.detail.contains("8s"),
            "{}",
            probe_failed.detail
        );
        let no_machine =
            tick_machine_watch(&mut state, Ok(&payload(None)), counter(&mut calls), now);
        assert_eq!(
            no_machine.skip_reason.as_deref(),
            Some("machine_unreadable")
        );
        assert!(no_machine.detail.contains("no machine object"));
        let unreadable_verdict = tick_machine_watch(
            &mut state,
            Ok(&payload(Some(machine("unreadable", 30)))),
            counter(&mut calls),
            now,
        );
        assert_eq!(
            unreadable_verdict.skip_reason.as_deref(),
            Some("machine_unreadable")
        );
        assert_eq!(calls.len(), 0);
    }

    #[test]
    fn the_notice_names_the_reason_then_the_top_three_by_their_own_strings() {
        let mut state = MachineWatchState::default();
        let mut calls: Vec<String> = Vec::new();
        let mut top: Vec<TopConsumer> = Vec::new();
        for (cpu, cmd) in [
            (
                54.8,
                "/Users/bb16/.fno/worktrees/footnote/x-d6ad/target/debug/fno-agents store-keeper",
            ),
            (38.8, "rustc --edition=2021"),
            (22.1, "sccache server"),
            (1.0, "tiny"),
        ] {
            top.push(TopConsumer {
                cpu_percent: cpu,
                command: cmd.to_string(),
            });
        }
        let reading =
            crate::spawn_gate::FootprintCausePayload::from_parts(Some(machine("hot", 30)), top);
        let now = Instant::now();
        tick_machine_watch(&mut state, Ok(&reading), counter(&mut calls), now);
        tick_machine_watch(&mut state, Ok(&reading), counter(&mut calls), now);
        assert_eq!(calls.len(), 1);
        let body = calls[0].clone();
        assert!(body.contains("machine 91.7%"), "{body}");
        assert!(
            body.contains("worktrees/footnote/x-d6ad/target/debug/fno-agents"),
            "{body}"
        );
        assert!(body.contains("rustc --edition=2021 (38.8%)"), "{body}");
        assert!(body.contains("sccache server (22.1%)"), "{body}");
        assert!(!body.contains("tiny"), "only the top three: {body}");
    }

    #[test]
    fn a_failed_notice_send_commits_no_throttle_state() {
        let mut state = MachineWatchState::default();
        let now = Instant::now();
        tick_machine_watch(
            &mut state,
            Ok(&payload(Some(machine("hot", 30)))),
            |_, _| false,
            now,
        );
        tick_machine_watch(
            &mut state,
            Ok(&payload(Some(machine("hot", 30)))),
            |_, _| false,
            now,
        );
        assert_eq!(state.last_notified, None);
    }
}
