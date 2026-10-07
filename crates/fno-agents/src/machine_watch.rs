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
pub const RUNAWAY_LOAD_PER_CORE: f64 = 4.0;
pub const RUNAWAY_LOAD_HOLD_SECS: u64 = 600;
pub const HOT_ESCALATION_SECS: u64 = 1800;
/// One-hour rolling baseline at the 300 s interval: 12 readings, armed at 4.
const BASELINE_WINDOW_TICKS: usize = 12;
const BASELINE_MIN_TICKS: usize = 4;
const PROCESS_RUNAWAY_FACTOR: f64 = 2.0;
const SWAP_RUNAWAY_FRACTION: f64 = 0.5;
/// Notice titles. The arm's runaway leg keys off the runaway one.
pub const RUNAWAY_TITLE: &str = "Machine overloaded";
pub const HOT_TITLE: &str = "Machine busy";

#[derive(Default)]
pub struct MachineWatchState {
    pub(crate) hot_streak: u32,
    pub(crate) calm_streak: u32,
    pub(crate) last_notified: Option<Instant>,
    pub(crate) prev_ticks: Option<crate::machine_sample::HostTicks>,
    /// Trailing process counts; the 1h baseline the runaway arm reads.
    pub(crate) recent_processes: Vec<(Instant, u64)>,
    pub(crate) absolute_load_since: Option<Instant>,
    pub(crate) hot_since: Option<Instant>,
    pub(crate) runaway_escalation_sent: bool,
    pub(crate) last_observed: Option<Instant>,
}

#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    pub load_per_core: f64,
    pub load_hold: Duration,
    pub hot_escalation: Duration,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            load_per_core: RUNAWAY_LOAD_PER_CORE,
            load_hold: Duration::from_secs(RUNAWAY_LOAD_HOLD_SECS),
            hot_escalation: Duration::from_secs(HOT_ESCALATION_SECS),
        }
    }
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
    pub verdict: String,
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
                "Machine out of memory: {} of {} GB of swap in use (fine is under half)",
                opt(sample.swap_used_gb),
                opt(sample.swap_total_gb)
            )
        } else {
            format!(
                "Machine overloaded: {} processes running, more than twice the usual {} over the last hour",
                sample.processes.unwrap_or_default(),
                baseline.unwrap_or_default()
            )
        };
        return ("runaway".into(), reason);
    }
    // Load is a queue, not work: macOS load counts short-lived process
    // churn, so only confirmed CPU busy makes a verdict hot.
    let busy_hot = sample.busy_fraction.is_some_and(|value| value > busy_band);
    let busy_readable = sample.busy_fraction.is_some() && sample.cores.is_some();
    let load_readable = sample.load_15m.is_some() && sample.cores.is_some();
    let verdict = if busy_hot {
        "hot"
    } else if busy_readable && load_readable {
        "calm"
    } else {
        "unreadable"
    };
    if verdict != "hot" {
        if let Some(issues) = sample
            .mux_issues
            .as_ref()
            .and_then(|value| value.as_array())
            .filter(|issues| !issues.is_empty())
        {
            // A mux finding is its own incident, not resource pressure: it
            // pages through its own verdict, never feeds the hot streak the
            // runaway brake escalates from, and never masks a hot reading.
            return (
                "mux".into(),
                format!("mux owner/socket issue: {} finding(s)", issues.len()),
            );
        }
    }
    let busy = busy_text(sample);
    let cores = sample
        .cores
        .map_or_else(|| "unknown".into(), |v| format!("{v:.0}"));
    let per_core = sample
        .load_15m
        .zip(sample.cores)
        .map_or_else(|| "unknown".into(), |(load, cores)| num(load / cores));
    let busy_cores = busy_cores_text(sample);
    let groups = top_groups_text(sample);
    let reason = if sample.busy_fraction.is_none() {
        "Machine unclear: CPU use is not readable (host CPU ticks unavailable)".to_string()
    } else {
        let word = match verdict {
            "hot" => "busy",
            "calm" => "calm",
            _ => "unclear",
        };
        format!(
            "Machine {word}: {busy_cores} of {cores} cores busy (CPU {busy}, fine is under {:.0}%), about {per_core} jobs per core waiting (fine is under {}); top groups {groups}; {} of {} processes running",
            busy_band * 100.0,
            num(load_band),
            sample.runnable.map_or_else(|| "unknown".into(), |v| v.to_string()),
            sample.processes.map_or_else(|| "unknown".into(), |v| v.to_string()),
        )
    };
    (verdict.into(), reason)
}

/// The busy-core count a person reads: `12`, or `unknown` when the host ticks
/// or the core count cannot answer. `busy_fraction * cores`, rounded: the
/// notice itself distinguishes "cores actually busy" from "jobs per core
/// waiting" (2026-10-03 investigation).
fn busy_cores_text(sample: &MachineSample) -> String {
    sample.busy_fraction.zip(sample.cores).map_or_else(
        || "unknown".to_string(),
        |(busy, cores)| format!("{:.0}", (busy * cores).round()),
    )
}

/// The busiest process-name groups, `rustc x4, python3 x15`, from the census
/// the sample already carries (sorted by count, so the first rows lead).
fn top_groups_text(sample: &MachineSample) -> String {
    let rows: Vec<String> = sample
        .top_names
        .as_ref()
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let name = row.get("name").and_then(|v| v.as_str())?;
            let count = row.get("count").and_then(|v| v.as_u64())?;
            Some(format!("{} x{count}", name))
        })
        .take(3)
        .collect();
    if rows.is_empty() {
        "unmeasured".to_string()
    } else {
        rows.join(", ")
    }
}

pub fn tick_machine_watch(
    state: &mut MachineWatchState,
    reading: Result<&MachineSample, &str>,
    notify: impl FnMut(&str, &str) -> bool,
    now: Instant,
    brake: impl FnMut(&MachineSample, &str),
) -> WatchOutcome {
    tick_machine_watch_with_thresholds(state, reading, notify, now, brake, Thresholds::default())
}

pub fn tick_machine_watch_with_thresholds(
    state: &mut MachineWatchState,
    reading: Result<&MachineSample, &str>,
    mut notify: impl FnMut(&str, &str) -> bool,
    now: Instant,
    mut brake: impl FnMut(&MachineSample, &str),
    thresholds: Thresholds,
) -> WatchOutcome {
    let sample = match reading {
        Ok(sample) => sample,
        Err(why) => {
            state.absolute_load_since = None;
            state.hot_since = None;
            state.hot_streak = 0;
            state.runaway_escalation_sent = false;
            state.last_observed = Some(now);
            return WatchOutcome {
                acted: 0,
                skip_reason: Some("machine_unreadable".into()),
                detail: short(&format!("probe: {why}")),
                verdict: "unreadable".into(),
            };
        }
    };
    if state.last_observed.is_some_and(|last| {
        now.saturating_duration_since(last) >= Duration::from_secs(MACHINE_WATCH_INTERVAL_S * 2)
    }) {
        state.absolute_load_since = None;
        state.hot_since = None;
        state.hot_streak = 0;
        state.runaway_escalation_sent = false;
    }
    state.last_observed = Some(now);
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
    let (mut verdict, mut reason) = decide(sample, busy_band, load_band, baseline);
    let mut forced_escalation = false;
    // The brake needs both: load per core over the threshold AND real CPU
    // busy over the busy band. macOS load counts process churn, so load
    // alone overstates pressure (2026-10-03: load 37-45 on 12 cores, about
    // 5 cores busy, 1115 short-lived processes).
    let cpu_confirms = sample.busy_fraction.is_some_and(|value| value > busy_band);
    let absolute_load_hot = cpu_confirms
        && sample
            .load_1m
            .zip(sample.cores)
            .is_some_and(|(load, cores)| cores > 0.0 && load / cores > thresholds.load_per_core);
    if absolute_load_hot {
        let since = state.absolute_load_since.get_or_insert(now);
        let elapsed = now.saturating_duration_since(*since);
        if elapsed >= thresholds.load_hold {
            verdict = "runaway".into();
            forced_escalation = true;
            reason = format!(
                "Machine overloaded: {} of {} cores busy (CPU {}, brake needs over {:.0}%), about {} jobs per core waiting (fine is under {}) for {}",
                busy_cores_text(sample),
                sample.cores.map_or_else(|| "unknown".into(), |v| format!("{v:.0}")),
                busy_text(sample),
                busy_band * 100.0,
                num(sample.load_1m.unwrap_or_default() / sample.cores.unwrap_or(1.0)),
                num(thresholds.load_per_core),
                span(elapsed),
            );
        } else {
            verdict = "hot".into();
            reason = format!(
                "Machine busy: {} of {} cores busy (CPU {}), about {} jobs per core waiting (fine is under {}) for {}; tests pause at {}",
                busy_cores_text(sample),
                sample.cores.map_or_else(|| "unknown".into(), |v| format!("{v:.0}")),
                busy_text(sample),
                num(sample.load_1m.unwrap_or_default() / sample.cores.unwrap_or(1.0)),
                num(thresholds.load_per_core),
                span(elapsed),
                span(thresholds.load_hold),
            );
        }
    } else {
        state.absolute_load_since = None;
    }
    if verdict == "hot" {
        let since = state.hot_since.get_or_insert(now);
        let elapsed = now.saturating_duration_since(*since);
        if elapsed >= thresholds.hot_escalation {
            verdict = "runaway".into();
            forced_escalation = true;
            reason = format!(
                "Machine overloaded: busy for {} and nothing has cooled it down",
                span(elapsed)
            );
        }
    } else {
        state.hot_since = None;
    }
    if !forced_escalation {
        state.runaway_escalation_sent = false;
    }
    match verdict.as_str() {
        "calm" => {
            state.calm_streak = state.calm_streak.saturating_add(1);
            state.hot_streak = 0;
            state.runaway_escalation_sent = false;
            WatchOutcome {
                acted: 0,
                skip_reason: Some("calm".into()),
                detail: short(&reason),
                verdict: "calm".into(),
            }
        }
        "runaway" => {
            state.calm_streak = 0;
            state.hot_streak = 0;
            // A runaway skips the hot debounce: one 507 s sample against the
            // 300 s interval made that debounce worth ~20 minutes of runway.
            if forced_escalation && !state.runaway_escalation_sent {
                state.last_notified = None;
            }
            brake(sample, &reason);
            let outcome = emit_notice(
                state,
                sample,
                &reason,
                now,
                RUNAWAY_TITLE,
                "runaway",
                &mut notify,
            );
            if forced_escalation && outcome.acted > 0 {
                state.runaway_escalation_sent = true;
            }
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
                    verdict: "hot".into(),
                };
            }
            emit_notice(state, sample, &reason, now, HOT_TITLE, "hot", &mut notify)
        }
        "mux" => {
            state.calm_streak = 0;
            state.hot_streak = 0;
            emit_notice(state, sample, &reason, now, HOT_TITLE, "mux", &mut notify)
        }
        _ => WatchOutcome {
            acted: 0,
            skip_reason: Some("machine_unreadable".into()),
            detail: short(&reason),
            verdict: "unreadable".into(),
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
    verdict: &str,
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
                    verdict: verdict.into(),
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
            verdict: verdict.into(),
        }
    } else {
        WatchOutcome {
            acted: 0,
            skip_reason: Some("notify_failed".into()),
            detail: short(&reason),
            verdict: verdict.into(),
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

/// The hold line for an unexpired brake, `None` when none is armed: the
/// spawn gate's own door on the brake. The arm attributes load before it
/// arms the brake, so a hold here means fno's own fan-out. A missing,
/// unreadable, or expired file holds nothing.
/// The hold line for an unexpired brake, `None` when none is armed: the
/// spawn gate's own door on the brake. The arm attributes load before it
/// arms the brake, so a hold here means fno's own fan-out. A missing,
/// unreadable, or expired file holds nothing.
pub fn brake_holds() -> Option<String> {
    // Test seam, same shape as the footprint probe's: with no pinned brake
    // file, tests never read the live machine's brake.
    #[cfg(test)]
    if std::env::var_os("FNO_MACHINE_BRAKE").is_none() {
        return None;
    }
    let text = std::fs::read_to_string(brake_path()).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let until = value.get("until_epoch")?.as_u64()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    (now < until).then(|| {
        let left = until - now;
        let reason = value
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unspecified");
        format!("machine runaway brake holds ({left}s left): {reason}")
    })
}

/// The runaway brake: a self-expiring file the spawn admission honors.
/// Best-effort - a failed write costs the refusal leg, never the notice.
/// The measured fleet/outside split rides the file, so a refusal names
/// whose load it was without a re-walk of the process table.
fn write_brake_file(
    sample: &MachineSample,
    reason: &str,
    split: Option<(f64, f64)>,
) -> Result<(), String> {
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
    let mut payload = serde_json::json!({
        "until_epoch": until,
        "reason": reason,
        "group": group,
        "processes": sample.processes,
        "swap_used_gb": sample.swap_used_gb,
    });
    if let Some((fleet, machine)) = split {
        payload["fleet_cores"] = serde_json::json!(fleet);
        payload["machine_cores"] = serde_json::json!(machine);
    }
    let path = brake_path();
    std::fs::write(&path, serde_json::to_string(&payload).unwrap_or_default())
        .map_err(|error| format!("{}: {error}", path.display()))
}

fn notice_body(reason: &str, sample: &MachineSample) -> String {
    let mut body = reason.to_string();
    let mut sessions: Vec<&serde_json::Value> = sample
        .sessions
        .as_ref()
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .collect();
    sessions.sort_by(|a, b| {
        b.get("cpu_pct")
            .and_then(|value| value.as_f64())
            .partial_cmp(&a.get("cpu_pct").and_then(|value| value.as_f64()))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let top_sessions: Vec<String> = sessions
        .into_iter()
        .take(3)
        .map(|row| {
            format!(
                "{} [{}] {:.1}% ({})",
                row.get("name")
                    .and_then(|value| value.as_str())
                    .unwrap_or("unknown worker"),
                row.get("session_id")
                    .and_then(|value| value.as_str())
                    .unwrap_or("unknown session"),
                row.get("cpu_pct")
                    .and_then(|value| value.as_f64())
                    .unwrap_or_default(),
                row.get("top_command")
                    .and_then(|value| value.as_str())
                    .unwrap_or("command unknown"),
            )
        })
        .collect();
    if !top_sessions.is_empty() {
        body.push_str("; top sessions: ");
        body.push_str(&top_sessions.join(", "));
    }
    if let Some(issues) = sample
        .mux_issues
        .as_ref()
        .and_then(|value| value.as_array())
        .filter(|issues| !issues.is_empty())
    {
        let findings: Vec<String> = issues
            .iter()
            .take(5)
            .map(|issue| {
                format!(
                    "{} socket={} owner={}",
                    issue
                        .get("kind")
                        .and_then(|value| value.as_str())
                        .unwrap_or("unknown"),
                    issue
                        .get("socket")
                        .and_then(|value| value.as_str())
                        .unwrap_or("unknown"),
                    issue
                        .get("owner_session")
                        .and_then(|value| value.as_str())
                        .unwrap_or("unknown"),
                )
            })
            .collect();
        body.push_str("; mux findings: ");
        body.push_str(&findings.join(", "));
    }
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

fn top_session_id(sample: &MachineSample) -> Option<String> {
    sample
        .sessions
        .as_ref()?
        .as_array()?
        .iter()
        .max_by(|a, b| {
            a.get("cpu_pct")
                .and_then(|value| value.as_f64())
                .partial_cmp(&b.get("cpu_pct").and_then(|value| value.as_f64()))
                .unwrap_or(std::cmp::Ordering::Equal)
        })?
        .get("session_id")?
        .as_str()
        .map(str::to_string)
}

fn opt(value: Option<f64>) -> String {
    value.map_or_else(|| "unmeasured".into(), |v| format!("{v:.1}"))
}
/// The busy fraction a person reads: `95.0%`, or `unavailable` when the
/// host ticks gave nothing.
fn busy_text(sample: &MachineSample) -> String {
    sample
        .busy_fraction
        .map_or_else(|| "unavailable".into(), |v| format!("{:.1}%", v * 100.0))
}
/// A reading in plain words: whole numbers lose the `.0`.
fn num(value: f64) -> String {
    if (value - value.round()).abs() < 0.05 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

/// A duration a person reads: seconds under 90, whole minutes above.
fn span(elapsed: Duration) -> String {
    match elapsed.as_secs() {
        s if s < 90 => format!("{s} seconds"),
        s => format!("{} minutes", (s + 30) / 60),
    }
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
        // Resume tests an expired hold paused; pause a test started mid-hold.
        if let Err(error) = crate::test_hold::reconcile(&home) {
            tracing::warn!(%error, "test hold reconcile failed");
        }
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
        let thresholds = Thresholds {
            load_per_core: crate::agents_config::runaway_load_per_core(&cwd),
            load_hold: Duration::from_secs(crate::agents_config::runaway_load_hold_seconds(&cwd)),
            hot_escalation: Duration::from_secs(crate::agents_config::hot_escalation_seconds(&cwd)),
        };
        sample.runaway_load_band_per_core = Some(thresholds.load_per_core);
        sample.runaway_load_hold_seconds = Some(thresholds.load_hold.as_secs());
        sample.hot_escalation_seconds = Some(thresholds.hot_escalation.as_secs());
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
        let journal = crate::loop_runtime::Journal::new_raw(
            home.events_jsonl(),
            crate::daemon::global_events_path(&home),
        );
        // Whose load the box is in, from the one footprint answerer the
        // spawn CPU axis already reads: unreadable or fleet-majority keeps
        // today's path, a minority share is outside load.
        let split = crate::spawn_gate::fleet_split();
        let source = crate::machine_load::classify(split);
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
            let stop_home = home.clone();
            let runtime = tokio::runtime::Handle::current();
            let brake_error = std::cell::RefCell::new(None);
            let tests_first = std::cell::RefCell::new(None::<String>);
            let held_first = std::cell::Cell::new(false);
            let extra_body = std::cell::RefCell::new(None::<String>);
            let outcome = tick_machine_watch_with_thresholds(
                &mut guard,
                Ok(&sample),
                |title, body| {
                    let mut body = body.to_string();
                    if let Some(error) = brake_error.borrow_mut().take() {
                        body.push_str(&format!("; spawn brake write failed: {error}"));
                    }
                    if let Some(extra) = extra_body.borrow_mut().take() {
                        body.push_str(&extra);
                    }
                    if let Some(held) = tests_first.borrow_mut().take() {
                        body.push_str(&format!(
                            "; {held}; the spawn brake and session stop wait for the next runaway tick"
                        ));
                    } else if title == RUNAWAY_TITLE
                        && source == crate::machine_load::LoadSource::Fleet
                    {
                        let stop_result = top_session_id(&sample).and_then(|session| {
                            runtime.block_on(crate::daemon::stop_session_for_home(
                                &stop_home, &session,
                            ))
                        });
                        body.push_str(&match stop_result {
                            Some((name, true)) => format!("; confirmed control stop: {name}"),
                            Some((name, false)) => format!("; control stop not confirmed: {name}"),
                            None => "; control stop unavailable: no unique registered top session"
                                .into(),
                        });
                    }
                    crate::operator_notice::notify_operator(title, &body, None)
                },
                Instant::now(),
                |sample, reason| {
                    match source {
                        crate::machine_load::LoadSource::Fleet => {
                            // fno's own load: reap its orphan test binaries
                            // first, then demote its live workers, then
                            // today's tests-first hold and the brake.
                            let reaped = crate::orphan_reap::reap_sweep_once(
                                true,
                                crate::orphan_reap::min_elapsed_secs(&cwd),
                            );
                            if !reaped.is_empty() {
                                *extra_body.borrow_mut() =
                                    Some(format!("; reaped {} orphan test binaries", reaped.len()));
                            }
                            let mut warnings = Vec::new();
                            for row in
                                crate::spawn_gate::live_rows(&home.registry_json(), &mut warnings)
                            {
                                if let Some(pid) = row.pid {
                                    crate::spawn_gate::qos_demote_pid(&cwd, pid);
                                }
                            }
                            // Tests yield first: with no stop armed, a
                            // runaway holds tests and brakes nothing else
                            // this tick.
                            match crate::test_hold::hold_for_runaway(
                                &stop_home,
                                reason,
                                MACHINE_BRAKE_HOLD_SECS,
                            ) {
                                Ok(Some(held)) => {
                                    *tests_first.borrow_mut() = Some(held);
                                    held_first.set(true);
                                    return;
                                }
                                Ok(None) => {}
                                Err(error) => tracing::warn!(%error, "tests-first hold failed"),
                            }
                            if let Err(error) = write_brake_file(sample, reason, split) {
                                tracing::error!(%error, "machine runaway brake write failed");
                                *brake_error.borrow_mut() = Some(error);
                            }
                        }
                        crate::machine_load::LoadSource::Outside => {
                            // Not fno's load: no hold, no brake file, no
                            // session stop. Ask the user about the top
                            // outside groups instead; the intake's subject
                            // dedup keeps it to one open question.
                            let fleet = crate::session_cost::owned_pids(
                                &sample.procs,
                                &crate::session_cost::session_roots(&home, &sample.procs),
                            );
                            let groups = crate::machine_load::outside_groups(&sample.procs, &fleet);
                            let stored: Vec<crate::machine_load::StoredGroup> = groups
                                .into_iter()
                                .map(|g| crate::machine_load::StoredGroup {
                                    name: g.name,
                                    cpu_pct: g.cpu_pct,
                                    rss_kb: g.rss_kb,
                                    bundle: g.bundle,
                                    pids: g.pids,
                                })
                                .collect();
                            let named: Vec<String> =
                                stored.iter().take(2).map(|g| g.name.clone()).collect();
                            if crate::machine_load::file_outside_question(&home, &cwd, stored) {
                                *extra_body.borrow_mut() = Some(format!(
                                    "; outside load: {}; asked the user before acting",
                                    named.join(", ")
                                ));
                            }
                        }
                    }
                },
                thresholds,
            );
            // The session stop rides the runaway notice, so the tests-first
            // notice must not start the throttle that would hold it back.
            if held_first.get() {
                guard.last_notified = None;
            }
            outcome
        };
        sample.verdict = Some(outcome.verdict.clone());
        let _ = journal.append(
            "machine_sample",
            sample.to_data(&outcome.verdict, busy_band, LOAD_PER_CORE_BAND),
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
        // Outside-load asks: apply each fresh answer once, and the first
        // calm tick resumes what a pause held.
        if let Some(applied) = crate::machine_load::poll_answers(&home, &cwd) {
            tracing::info!(applied = %applied, "machine-load answer applied");
        }
        if outcome.verdict == "calm" {
            if let Some(resumed) = crate::machine_load::resume_paused() {
                tracing::info!(resumed = %resumed, "paused pids resumed");
            }
        }
        // The budget ask rides a calm tick: a runaway tick is already
        // acting, and the intake dedup keeps it to one open question.
        if outcome.verdict == "calm" {
            crate::machine_load::file_budget_question(&home, &cwd, &sample);
        }
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
    fn load_can_make_machine_hot_only_when_cpu_confirms() {
        let (verdict, reason) = decide(&sample(Some(0.95), Some(363.0)), 0.9, 10.0, None);
        assert_eq!(verdict, "hot");
        assert!(
            reason.contains("11 of 12 cores busy"),
            "the reason names the busy cores: {reason}"
        );
        assert!(
            reason.contains("top groups unmeasured"),
            "a sample with no census says so instead of inventing groups: {reason}"
        );
        assert!(
            reason.contains("about 30.2 jobs per core waiting (fine is under 10)"),
            "{reason}"
        );
        // Churn-inflated load with idle cores reads calm, never hot.
        let (verdict, reason) = decide(&sample(Some(0.4), Some(363.0)), 0.9, 10.0, None);
        assert_eq!(verdict, "calm");
        assert!(
            reason.contains("about 30.2 jobs per core waiting"),
            "{reason}"
        );
        // An unreadable reading never reads as calm, and load alone with
        // unreadable busy cannot read hot.
        let (verdict, _) = decide(&sample(None, Some(2.0)), 0.9, 10.0, None);
        assert_eq!(verdict, "unreadable");
        let (verdict, _) = decide(&sample(None, Some(363.0)), 0.9, 10.0, None);
        assert_eq!(verdict, "unreadable");

        // A mux finding is its own verdict: it pages through the notice
        // throttle, never brakes, and never feeds the hot streak the brake
        // escalates from.
        let mut muxed = sample(Some(0.4), Some(150.0));
        muxed.load_1m = Some(96.0);
        muxed.mux_issues = Some(serde_json::json!([{ "kind": "duplicate-server" }]));
        let (verdict, reason) = decide(&muxed, 0.9, 10.0, None);
        assert_eq!(verdict, "mux");
        assert!(reason.contains("mux owner/socket issue"), "{reason}");
        let mut mux_state = MachineWatchState::default();
        let mux_start = Instant::now();
        let mut mux_notices = 0;
        let mut mux_brakes = 0;
        for elapsed in [0, 1803] {
            let outcome = tick_machine_watch_with_thresholds(
                &mut mux_state,
                Ok(&muxed),
                |_, _| {
                    mux_notices += 1;
                    true
                },
                mux_start + Duration::from_secs(elapsed),
                |_, _| mux_brakes += 1,
                Thresholds::default(),
            );
            assert_eq!(outcome.verdict, "mux", "tick at {elapsed}s");
        }
        assert_eq!(mux_notices, 2, "mux pages through the notice throttle");
        assert_eq!(mux_brakes, 0, "a mux finding never arms the brake");

        // Saturation outranks the mux page: a hot reading with a mux
        // finding stays hot, so the brake clock keeps running.
        let mut hot_and_muxed = sample(Some(0.95), Some(150.0));
        hot_and_muxed.mux_issues = Some(serde_json::json!([{ "kind": "duplicate-server" }]));
        let (verdict, _) = decide(&hot_and_muxed, 0.9, 10.0, None);
        assert_eq!(verdict, "hot");

        // hot -> mux -> hot: the mux tick resets the streak, so the
        // isolated hot sample debounces instead of notifying on one reading.
        let mut seq_state = MachineWatchState::default();
        let seq_start = Instant::now();
        let mut seq_notifies = 0;
        let mut last_skip = String::new();
        let mux_sample = || {
            let mut s = sample(Some(0.4), Some(150.0));
            s.load_1m = Some(96.0);
            s.mux_issues = Some(serde_json::json!([{ "kind": "duplicate-server" }]));
            s
        };
        for elapsed in [0, 300, 600] {
            let sample_at = if elapsed == 300 {
                mux_sample()
            } else {
                sample(Some(0.95), Some(1.0))
            };
            let outcome = tick_machine_watch_with_thresholds(
                &mut seq_state,
                Ok(&sample_at),
                |_, _| {
                    seq_notifies += 1;
                    true
                },
                seq_start + Duration::from_secs(elapsed),
                |_, _| {},
                Thresholds::default(),
            );
            last_skip = outcome.skip_reason.unwrap_or_default();
        }
        assert_eq!(seq_notifies, 1, "only the mux tick pages");
        assert_eq!(last_skip, "debouncing", "the isolated hot sample debounces");

        // Churn walk: sustained load with idle busy never brakes. The
        // 2026-10-03 shape: 8 jobs per core waiting, about 5 of 12 cores
        // busy, all of it short-lived process churn.
        let mut state = MachineWatchState::default();
        let churn = || {
            let mut s = sample(Some(0.4), Some(150.0));
            s.load_1m = Some(96.0);
            s
        };
        let start = Instant::now();
        let mut brakes = 0;
        for elapsed in [0, 300, 603, 903, 1803] {
            let outcome = tick_machine_watch_with_thresholds(
                &mut state,
                Ok(&churn()),
                |_, _| true,
                start + Duration::from_secs(elapsed),
                |_, _| brakes += 1,
                Thresholds::default(),
            );
            assert_eq!(outcome.verdict, "calm", "tick at {elapsed}s");
            assert_eq!(outcome.acted, 0, "tick at {elapsed}s");
        }
        // A busy tick reopens the window; the next churn tick closes it, so
        // the hold never accumulates across mixed samples.
        let confirmed = || {
            let mut s = sample(Some(0.95), Some(150.0));
            s.load_1m = Some(96.0);
            s
        };
        for (elapsed, busy_now) in [(2103, true), (2403, false), (2703, true), (3003, false)] {
            let sample_at = if busy_now { confirmed() } else { churn() };
            let outcome = tick_machine_watch_with_thresholds(
                &mut state,
                Ok(&sample_at),
                |_, _| true,
                start + Duration::from_secs(elapsed),
                |_, _| brakes += 1,
                Thresholds::default(),
            );
            assert_ne!(outcome.verdict, "runaway", "tick at {elapsed}s");
        }
        assert_eq!(brakes, 0, "no brake without confirmed busy");

        // Confirmed escalation walk: the same load with busy confirmed still
        // escalates after the hold, and the page names both numbers.
        let mut state = MachineWatchState::default();
        let mut absolute = sample(Some(0.95), Some(1.0));
        absolute.load_1m = Some(96.0); // 8 per core, above the absolute 4 band.
        let start = Instant::now();
        let mut notices = 0;
        let mut brakes = 0;
        let mut last = (String::new(), String::new());
        for elapsed in [0, 300, 603] {
            let outcome = tick_machine_watch_with_thresholds(
                &mut state,
                Ok(&absolute),
                |title, body| {
                    notices += 1;
                    last = (title.to_string(), body.to_string());
                    true
                },
                start + Duration::from_secs(elapsed),
                |_, _| brakes += 1,
                Thresholds::default(),
            );
            if elapsed == 603 {
                assert_eq!(outcome.verdict, "runaway");
            }
        }
        // The page a person reads: plain words, busy cores first, both
        // numbers, the scale built in.
        assert_eq!(last.0, RUNAWAY_TITLE);
        assert!(
            last.1.starts_with(
                "Machine overloaded: 11 of 12 cores busy (CPU 95.0%, brake needs over 90%), about 8 jobs per core waiting (fine is under 4) for 10 minutes"
            ),
            "{}",
            last.1
        );
        assert_eq!(brakes, 1);
        assert_eq!(
            notices, 2,
            "runaway escalation pages despite the hot notice throttle"
        );
    }

    #[test]
    fn two_hot_samples_notify_once() {
        let mut state = MachineWatchState::default();
        let mut calls = 0;
        let mut brakes = 0;
        let hot = sample(Some(1.0), Some(1.0));
        let start = Instant::now();
        assert_eq!(
            tick_machine_watch(
                &mut state,
                Ok(&hot),
                |_, _| {
                    calls += 1;
                    true
                },
                start,
                |_, _| brakes += 1
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
                start + Duration::from_secs(300),
                |_, _| brakes += 1
            )
            .acted,
            1
        );
        assert_eq!(calls, 1);
        let mut escalated = None;
        for elapsed in (600..=HOT_ESCALATION_SECS).step_by(300) {
            escalated = Some(tick_machine_watch(
                &mut state,
                Ok(&hot),
                |_, _| {
                    calls += 1;
                    true
                },
                start + Duration::from_secs(elapsed),
                |_, _| brakes += 1,
            ));
        }
        let escalated = escalated.unwrap();
        assert_eq!(escalated.verdict, "runaway");
        assert_eq!(
            escalated.acted, 1,
            "escalation bypasses the earlier hot notice throttle"
        );
        assert_eq!(brakes, 1);
        assert_eq!(calls, 2);
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
        assert!(reason.contains("twice the usual 1200"), "{reason}");
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
        let actions = std::cell::RefCell::new(Vec::new());
        let outcome = tick_machine_watch(
            &mut state,
            Ok(&s),
            |_, _| {
                notify_calls += 1;
                actions.borrow_mut().push("notify");
                true
            },
            Instant::now(),
            |s, r| {
                brake_calls += 1;
                actions.borrow_mut().push("brake");
                write_brake_file(s, r, Some((6.0, 8.0))).unwrap();
            },
        );
        assert_eq!(outcome.acted, 1, "no debounce on a runaway");
        assert_eq!(notify_calls, 1);
        assert_eq!(brake_calls, 1, "the brake writes on the first tick");
        assert_eq!(*actions.borrow(), vec!["brake", "notify"]);
        assert!(
            outcome.detail.contains("out of memory"),
            "{}",
            outcome.detail
        );
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
        assert!(stored["reason"].as_str().unwrap().contains("out of memory"));
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
