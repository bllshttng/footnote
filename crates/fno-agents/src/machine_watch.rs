//! The daemon's 300-second machine sample arm.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::machine_sample::MachineSample;
use crate::paths::AgentsHome;

pub const MACHINE_HOT_SAMPLES: u32 = 2;
pub const MACHINE_WATCH_INTERVAL_S: u64 = 300;
pub const LOAD_PER_CORE_BAND: f64 = 10.0;

/// The runaway brake: written when a runaway notice sends, honored by spawn
/// admission for 30 minutes. Named for the notify-signals file pattern.
pub const MACHINE_BRAKE_NAME: &str = "machine-brake.json";
pub const MACHINE_BRAKE_HOLD_SECS: u64 = 1800;
/// One-hour rolling baseline at the 300 s interval: 12 readings, armed at 4.
const BASELINE_WINDOW_TICKS: usize = 12;
const BASELINE_MIN_TICKS: usize = 4;
const PROCESS_RUNAWAY_FACTOR: f64 = 2.0;
const SWAP_RUNAWAY_FRACTION: f64 = 0.5;

#[derive(Default)]
pub struct MachineWatchState {
    pub(crate) hot_streak: u32,
    pub(crate) calm_streak: u32,
    pub(crate) last_notified: Option<Instant>,
    pub(crate) prev_ticks: Option<crate::machine_sample::HostTicks>,
    /// Trailing process counts; the 1h baseline the runaway arm reads.
    pub(crate) recent_processes: Vec<(Instant, u64)>,
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

pub fn decide(
    sample: &MachineSample,
    busy_band: f64,
    load_band: f64,
    baseline: Option<u64>,
) -> (String, String) {
    let swap_runaway = sample
        .swap_used_gb
        .zip(sample.swap_total_gb)
        .is_some_and(|(used, total)| total > 0.0 && used / total > SWAP_RUNAWAY_FRACTION);
    let process_runaway = sample
        .processes
        .zip(baseline.filter(|base| *base > 0))
        .is_some_and(|(processes, base)| processes as f64 > PROCESS_RUNAWAY_FACTOR * base as f64);
    if swap_runaway || process_runaway {
        let reason = if swap_runaway {
            format!(
                "machine runaway: swap {} of {} GB crosses the 50% band",
                opt(sample.swap_used_gb),
                opt(sample.swap_total_gb)
            )
        } else {
            format!(
                "machine runaway: {} processes cross 2x the 1h baseline {}",
                sample.processes.unwrap_or_default(),
                baseline.unwrap_or_default()
            )
        };
        return ("runaway".into(), reason);
    }
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
    mut brake: impl FnMut(&MachineSample, &str),
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
    // The current reading never sits on its own jury: baseline first, push after.
    let baseline = process_baseline(&state.recent_processes, now);
    if let Some(processes) = sample.processes {
        state.recent_processes.push((now, processes));
        if let Some(hour_ago) = now.checked_sub(Duration::from_secs(3600)) {
            state.recent_processes.retain(|(t, _)| *t >= hour_ago);
        }
        while state.recent_processes.len() > BASELINE_WINDOW_TICKS {
            state.recent_processes.remove(0);
        }
    }
    let busy_band = sample.busy_band.unwrap_or(0.9);
    let load_band = sample.load_band_per_core.unwrap_or(LOAD_PER_CORE_BAND);
    let (verdict, reason) = decide(sample, busy_band, load_band, baseline);
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
        "runaway" => {
            state.calm_streak = 0;
            state.hot_streak = 0;
            // A runaway skips the hot debounce: one 507 s sample against the
            // 300 s interval made that debounce worth ~20 minutes of runway.
            let outcome = emit_notice(
                state,
                sample,
                &reason,
                now,
                "machine_watch: box runaway",
                &mut notify,
            );
            // The brake refreshes on every runaway tick, notice sent or not:
            // the 30m hold must outlive the notice throttle, or a sustained
            // runaway reopens the gate mid-fire.
            brake(sample, &reason);
            outcome
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
            emit_notice(
                state,
                sample,
                &reason,
                now,
                "machine_watch: box hot",
                &mut notify,
            )
        }
        _ => WatchOutcome {
            acted: 0,
            skip_reason: Some("machine_unreadable".into()),
            detail: short(&reason),
        },
    }
}

/// The throttle-plus-notify half both hot and runaway verdicts share.
fn emit_notice(
    state: &mut MachineWatchState,
    sample: &MachineSample,
    reason: &str,
    now: Instant,
    title: &str,
    notify: &mut impl FnMut(&str, &str) -> bool,
) -> WatchOutcome {
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
    if notify(title, &notice_body(reason, sample)) {
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

/// Mean process count over the trailing hour, armed at 4 readings. `None`
/// until then: a cold arm never fires the process runaway leg, and the swap
/// leg covers the gap.
pub fn process_baseline(window: &[(Instant, u64)], now: Instant) -> Option<u64> {
    let hour_ago = now.checked_sub(Duration::from_secs(3600))?;
    let fresh: Vec<u64> = window
        .iter()
        .filter(|(t, _)| *t >= hour_ago)
        .map(|(_, p)| *p)
        .collect();
    if fresh.len() < BASELINE_MIN_TICKS {
        return None;
    }
    Some((fresh.iter().sum::<u64>() / fresh.len() as u64).max(1))
}

/// Where admission looks for the brake. `FNO_MACHINE_BRAKE` overrides the
/// full path; the default follows the notify-signals file pattern.
pub(crate) fn brake_path() -> PathBuf {
    if let Some(v) = std::env::var_os("FNO_MACHINE_BRAKE").filter(|v| !v.is_empty()) {
        return PathBuf::from(v);
    }
    let home = std::env::var_os("HOME").unwrap_or_else(|| std::ffi::OsString::from("."));
    crate::state_layout::place(&PathBuf::from(home).join(".fno"), MACHINE_BRAKE_NAME)
}

/// The runaway brake: a self-expiring file the spawn admission honors.
/// Best-effort - a failed write costs the refusal leg, never the notice.
fn write_brake_file(sample: &MachineSample, reason: &str) {
    let until = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() + MACHINE_BRAKE_HOLD_SECS)
        .unwrap_or_default();
    let group = sample
        .top_names
        .as_ref()
        .and_then(|v| v.as_array())
        .and_then(|rows| rows.first())
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let payload = serde_json::json!({
        "until_epoch": until,
        "reason": reason,
        "group": group,
        "processes": sample.processes,
        "swap_used_gb": sample.swap_used_gb,
    });
    let _ = std::fs::write(
        brake_path(),
        serde_json::to_string(&payload).unwrap_or_default(),
    );
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
    if let Some(top) = sample
        .top_names
        .as_ref()
        .and_then(|v| v.as_array())
        .and_then(|rows| rows.first())
    {
        let name = top.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        let count = top.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
        let ppid = top.get("ppid").and_then(|v| v.as_u64()).unwrap_or(0);
        body.push_str(&format!("; largest group {name} x{count} (ppid {ppid})"));
    }
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
        match crate::session_cost::price(&home, &sample.procs) {
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
        let baseline = {
            let guard = state.lock().unwrap_or_else(|e| e.into_inner());
            process_baseline(&guard.recent_processes, Instant::now())
        };
        if let Err(error) = crate::machine_sample::maybe_capture_process_snapshot(
            &home,
            sample.procs.len(),
            baseline,
            || {
                let output = std::process::Command::new("ps")
                    .args(["-Ao", "pid,ppid,rss,etime,command"])
                    .output()?;
                if !output.status.success() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "ps exited unsuccessfully",
                    ));
                }
                Ok(output.stdout)
            },
        ) {
            tracing::warn!(%error, "machine process snapshot failed");
        }
        let (verdict, _) = decide(&sample, busy_band, LOAD_PER_CORE_BAND, baseline);
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
                |sample, reason| write_brake_file(sample, reason),
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
        let (verdict, reason) = decide(&sample(Some(0.487), Some(363.0)), 0.9, 10.0, None);
        assert_eq!(verdict, "hot");
        assert!(reason.contains("30.2 per core crosses load band 10"));
    }

    #[test]
    fn unreadable_never_reads_calm() {
        let (verdict, _) = decide(&sample(None, Some(2.0)), 0.9, 10.0, None);
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
                Instant::now(),
                |_, _| {}
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
                Instant::now(),
                |_, _| {}
            )
            .acted,
            1
        );
        assert_eq!(calls, 1);
    }

    #[test]
    fn the_runaway_arms_fire_on_swap_and_on_an_armed_process_baseline() {
        let mut s = sample(None, None);
        s.swap_used_gb = Some(50.0);
        s.swap_total_gb = Some(95.0);
        let (verdict, reason) = decide(&s, 0.9, 10.0, None);
        assert_eq!(verdict, "runaway", "swap needs no history");
        assert!(reason.contains("swap"), "reason names the signal: {reason}");
        let mut s = sample(None, None);
        s.processes = Some(2500);
        let (verdict, _) = decide(&s, 0.9, 10.0, None);
        assert_eq!(verdict, "unreadable", "no baseline, no process arm");
        let (verdict, reason) = decide(&s, 0.9, 10.0, Some(1200));
        assert_eq!(verdict, "runaway");
        assert!(reason.contains("baseline"), "{reason}");
        let now = Instant::now();
        let window: Vec<(Instant, u64)> = (0..3)
            .map(|i| (now - Duration::from_secs(300 * (i as u64 + 1)), 1000 + i))
            .collect();
        assert_eq!(process_baseline(&window, now), None);
        let window: Vec<(Instant, u64)> = (0..4)
            .map(|i| (now - Duration::from_secs(300 * (i as u64 + 1)), 1000 + i))
            .collect();
        assert_eq!(process_baseline(&window, now), Some(1001));
    }

    #[test]
    fn a_runaway_ticks_page_immediately_and_the_brake_names_its_group() {
        let mut state = MachineWatchState::default();
        let mut s = sample(None, None);
        s.swap_used_gb = Some(50.0);
        s.swap_total_gb = Some(95.0);
        s.processes = Some(9000);
        s.top_names = Some(serde_json::json!([
            {"name": "git", "count": 8000, "ppid": 42}
        ]));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("brake.json");
        std::env::set_var("FNO_MACHINE_BRAKE", &path);
        let mut notify_calls = 0;
        let mut brake_calls = 0;
        let outcome = tick_machine_watch(
            &mut state,
            Ok(&s),
            |_, _| {
                notify_calls += 1;
                true
            },
            Instant::now(),
            |s, r| {
                brake_calls += 1;
                write_brake_file(s, r);
            },
        );
        assert_eq!(outcome.acted, 1, "no debounce on a runaway");
        assert_eq!(notify_calls, 1);
        assert_eq!(brake_calls, 1, "the brake writes on the first tick");
        assert!(outcome.detail.contains("runaway"), "{}", outcome.detail);
        let stored: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(stored["group"]["name"], "git");
        assert_eq!(stored["processes"], 9000);
        assert!(
            stored["until_epoch"].as_u64().unwrap()
                > std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
        );
        assert!(stored["reason"].as_str().unwrap().contains("runaway"));
        // A throttled second tick still refreshes the brake: the hold must
        // outlive the notice throttle on a sustained runaway.
        let outcome = tick_machine_watch(
            &mut state,
            Ok(&s),
            |_, _| {
                notify_calls += 1;
                true
            },
            Instant::now(),
            |_, _| brake_calls += 1,
        );
        std::env::remove_var("FNO_MACHINE_BRAKE");
        assert_eq!(outcome.acted, 0, "second notice is throttled");
        assert_eq!(notify_calls, 1);
        assert_eq!(brake_calls, 2);
    }
}
