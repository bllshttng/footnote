//! The daemon's 300-second machine sample arm.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::machine_sample::MachineSample;
use crate::paths::AgentsHome;

pub const MACHINE_HOT_SAMPLES: u32 = 2;
pub const MACHINE_WATCH_INTERVAL_S: u64 = 300;
pub const LOAD_PER_CORE_BAND: f64 = 10.0;

#[derive(Default)]
pub struct MachineWatchState {
    pub(crate) hot_streak: u32,
    pub(crate) calm_streak: u32,
    pub(crate) last_notified: Option<Instant>,
    pub(crate) prev_ticks: Option<crate::machine_sample::HostTicks>,
}

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

pub struct WatchOutcome {
    pub acted: u64,
    pub skip_reason: Option<String>,
    pub detail: String,
}

pub fn decide(sample: &MachineSample, busy_band: f64, load_band: f64) -> (String, String) {
    let busy_hot = sample.busy_fraction.is_some_and(|value| value > busy_band);
    let load_hot = sample
        .load_15m
        .zip(sample.cores)
        .is_some_and(|(load, cores)| cores > 0.0 && load / cores > load_band);
    let busy_readable = sample.busy_fraction.is_some() && sample.cores.is_some();
    let load_readable = sample.load_15m.is_some() && sample.cores.is_some();
    let verdict = if busy_hot || load_hot {
        "hot"
    } else if busy_readable && load_readable {
        "calm"
    } else {
        "unreadable"
    };
    let busy = sample
        .busy_fraction
        .map_or_else(|| "unavailable".into(), |v| format!("{:.1}%", v * 100.0));
    let cores = sample
        .cores
        .map_or_else(|| "unavailable".into(), |v| format!("{v:.2}"));
    let busy_cores = sample.busy_fraction.zip(sample.cores).map_or_else(
        || "unavailable".into(),
        |(busy, cores)| format!("{:.3}", busy * cores),
    );
    let per_core = sample.load_15m.zip(sample.cores).map_or_else(
        || "unavailable".into(),
        |(load, cores)| format!("{:.1}", load / cores),
    );
    let busy_relation = if busy_hot { "crosses" } else { "of" };
    let load_relation = if load_hot { "crosses" } else { "of" };
    let reason = if sample.busy_fraction.is_none() {
        "machine busy unmeasured (host CPU ticks unavailable)".to_string()
    } else {
        format!(
            "machine {busy} {busy_relation} band {:.0}% ({busy_cores} of {cores} cores) -> {verdict}; load_15m {}, {} runnable of {} processes; {per_core} per core {load_relation} load band {load_band:.0}",
            busy_band * 100.0,
            sample.load_15m.map_or_else(|| "unavailable".into(), |v| format!("{v:.1}")),
            sample.runnable.map_or_else(|| "None".into(), |v| v.to_string()),
            sample.processes.map_or_else(|| "None".into(), |v| v.to_string()),
        )
    };
    (verdict.into(), reason)
}

pub fn tick_machine_watch(
    state: &mut MachineWatchState,
    reading: Result<&MachineSample, &str>,
    mut notify: impl FnMut(&str, &str) -> bool,
    now: Instant,
) -> WatchOutcome {
    let sample = match reading {
        Ok(sample) => sample,
        Err(why) => {
            return WatchOutcome {
                acted: 0,
                skip_reason: Some("machine_unreadable".into()),
                detail: short(&format!("probe: {why}")),
            }
        }
    };
    let busy_band = sample.busy_band.unwrap_or(0.9);
    let load_band = sample.load_band_per_core.unwrap_or(LOAD_PER_CORE_BAND);
    let (verdict, reason) = decide(sample, busy_band, load_band);
    match verdict.as_str() {
        "calm" => {
            state.calm_streak = state.calm_streak.saturating_add(1);
            state.hot_streak = 0;
            WatchOutcome {
                acted: 0,
                skip_reason: Some("calm".into()),
                detail: short(&reason),
            }
        }
        "hot" => {
            state.hot_streak = state.hot_streak.saturating_add(1);
            state.calm_streak = 0;
            if state.hot_streak < MACHINE_HOT_SAMPLES {
                return WatchOutcome {
                    acted: 0,
                    skip_reason: Some("debouncing".into()),
                    detail: short(&format!(
                        "{}/{} hot: {reason}",
                        state.hot_streak, MACHINE_HOT_SAMPLES
                    )),
                };
            }
            let throttle = Duration::from_secs(sample.throttle_minutes.saturating_mul(60));
            if let Some(last) = state.last_notified {
                if let Some(held) = now.checked_duration_since(last) {
                    if held < throttle {
                        return WatchOutcome {
                            acted: 0,
                            skip_reason: Some("throttled".into()),
                            detail: short(&format!(
                                "hot, notice held {}s more: {reason}",
                                (throttle - held).as_secs()
                            )),
                        };
                    }
                }
            }
            if notify("machine_watch: box hot", &notice_body(&reason, sample)) {
                state.last_notified = Some(now);
                WatchOutcome {
                    acted: 1,
                    skip_reason: None,
                    detail: short(&format!("notified: {reason}")),
                }
            } else {
                WatchOutcome {
                    acted: 0,
                    skip_reason: Some("notify_failed".into()),
                    detail: short(&reason),
                }
            }
        }
        _ => WatchOutcome {
            acted: 0,
            skip_reason: Some("machine_unreadable".into()),
            detail: short(&reason),
        },
    }
}

fn notice_body(reason: &str, sample: &MachineSample) -> String {
    let mut body = reason.to_string();
    let top: Vec<String> = sample
        .top_cpu
        .iter()
        .map(|row| format!("{} ({:.1}%)", row.command, row.cpu_pct))
        .collect();
    if !top.is_empty() {
        body.push_str(" top: ");
        body.push_str(&top.join(", "));
    }
    body.push_str(&format!(
        "; compressor {} GB, swap {} of {} GB, {} zombies",
        opt(sample.compressor_gb),
        opt(sample.swap_used_gb),
        opt(sample.swap_total_gb),
        sample
            .zombies
            .map_or_else(|| "unmeasured".into(), |v| v.to_string())
    ));
    body
}

fn opt(value: Option<f64>) -> String {
    value.map_or_else(|| "unmeasured".into(), |v| format!("{v:.1}"))
}
fn short(text: &str) -> String {
    text.chars().take(200).collect()
}

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
        let (mut sample, ticks) = {
            let previous = state.lock().unwrap_or_else(|e| e.into_inner()).prev_ticks;
            crate::machine_sample::read(&home, previous)
        };
        let cwd = std::env::current_dir().unwrap_or_default();
        let busy_band = crate::agents_config::config_lookup(
            &cwd,
            &["resource_meter.thresholds.cpu_busy_fraction"],
        )
        .and_then(|v| v.as_float())
        .filter(|v| *v > 0.0 && *v <= 1.0)
        .unwrap_or(0.9);
        sample.busy_band = Some(busy_band);
        sample.load_band_per_core = Some(LOAD_PER_CORE_BAND);
        match crate::session_cost::price(&home.root().to_path_buf(), &sample.procs) {
            Ok(value) => {
                sample.sessions = value.get("sessions").cloned();
                sample.unresolved = Some(
                    value
                        .get("unresolved")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!([])),
                );
                sample.top_rss = value.get("top_rss").cloned();
            }
            Err(error) => sample.sessions_error = Some(error),
        }
        let (verdict, _) = decide(&sample, busy_band, LOAD_PER_CORE_BAND);
        sample.verdict = Some(verdict.clone());
        let journal = crate::loop_runtime::Journal::new_raw(
            home.events_jsonl(),
            crate::daemon::global_events_path(&home),
        );
        let _ = journal.append(
            "machine_sample",
            sample.to_data(&verdict, busy_band, LOAD_PER_CORE_BAND),
        );
        let outcome = {
            let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
            guard.prev_ticks = ticks;
            sample.throttle_minutes = crate::agents_config::config_lookup(
                &cwd,
                &["resource_meter.notifications.throttle_minutes"],
            )
            .and_then(|v| v.as_integer())
            .unwrap_or(60)
            .clamp(0, 10080) as u64;
            tick_machine_watch(
                &mut guard,
                Ok(&sample),
                |title, body| crate::operator_notice::notify_operator(title, body, None),
                Instant::now(),
            )
        };
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

    fn sample(busy: Option<f64>, load: Option<f64>) -> MachineSample {
        MachineSample {
            busy_fraction: busy,
            cores: Some(12.0),
            load_15m: load,
            runnable: Some(1),
            processes: Some(2),
            busy_band: Some(0.9),
            load_band_per_core: Some(10.0),
            throttle_minutes: 30,
            ..Default::default()
        }
    }

    #[test]
    fn load_can_make_machine_hot() {
        let (verdict, reason) = decide(&sample(Some(0.487), Some(363.0)), 0.9, 10.0);
        assert_eq!(verdict, "hot");
        assert!(reason.contains("30.2 per core crosses load band 10"));
    }

    #[test]
    fn unreadable_never_reads_calm() {
        let (verdict, _) = decide(&sample(None, Some(2.0)), 0.9, 10.0);
        assert_eq!(verdict, "unreadable");
    }

    #[test]
    fn two_hot_samples_notify_once() {
        let mut state = MachineWatchState::default();
        let mut calls = 0;
        let hot = sample(Some(1.0), Some(1.0));
        assert_eq!(
            tick_machine_watch(
                &mut state,
                Ok(&hot),
                |_, _| {
                    calls += 1;
                    true
                },
                Instant::now()
            )
            .acted,
            0
        );
        assert_eq!(
            tick_machine_watch(
                &mut state,
                Ok(&hot),
                |_, _| {
                    calls += 1;
                    true
                },
                Instant::now()
            )
            .acted,
            1
        );
        assert_eq!(calls, 1);
    }
}
