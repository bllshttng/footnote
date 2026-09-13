//! The arm_watch arm: one watcher reads the arms table's own verdict and tells the operator when an arm stays broken past `[notify] arm_failing_after_s` (default 1800).
//!
//! The trigger reads the table's verdict, it computes none of its own: rows come from `tick_ledger::read_arms` + `explain_with_trace`, and the only state this arm adds is the dedupe token in the signal store. One rolled-up set per notice, deduped on which arms are broken and since when, because arms on the pr-watch tier fail together and a per-arm pager would cry all day.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::paths::AgentsHome;
use crate::tick_ledger::{ArmStatus, DaemonFacts};

/// The arm's own beat, matching its `KNOWN_ARMS` row.
pub const ARM_WATCH_INTERVAL_S: u64 = 300;

/// The dedupe key in the notify signal store.
const SIGNAL_KEY: &str = "arm_failing";

/// The arm as the daemon holds it: cadence stamp plus one-in-flight gate. The dedupe memory lives in the signal store, so it survives a daemon restart. The config cwd rides at construction, so the daemon declares and calls this arm in one line each.
pub struct Arm {
    config_cwd: PathBuf,
    last_tick: Mutex<Option<Instant>>,
    in_flight: Arc<AtomicBool>,
}

impl Arm {
    pub fn new(config_cwd: PathBuf) -> Self {
        Self {
            config_cwd,
            last_tick: Mutex::new(None),
            in_flight: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// One tick's outcome: the tick row's `acted`, `skip_reason` and `detail`.
pub struct WatchOutcome {
    pub acted: u64,
    pub skip_reason: Option<String>,
    pub detail: String,
}

/// One pass of the arm body, with the rows handed in and the send handed in. The overdue set, the token, the body and the store writes are all this function's; the caller owns the reads and the send. An empty set forgets the stored token and stays silent: recovery is the designed quiet, like the board lane.
pub fn tick_arm_watch(
    rows: &[ArmStatus],
    threshold_s: u64,
    store: &Path,
    now_unix: u64,
    send: impl FnOnce(&str, &str) -> bool,
) -> WatchOutcome {
    let overdue: Vec<&ArmStatus> = rows
        .iter()
        .filter(|row| {
            if row.arm == "arm_watch" {
                // Its own death must not page about itself; its tick row is
                // the evidence and the readout is the next reader's input.
                return false;
            }
            let failing_overdue = row.failing && row.failing_for_s.is_none_or(|s| s >= threshold_s);
            let stale_overdue = row.stale
                && matches!(
                    row.cause.as_deref(),
                    Some("scheduler_down") | Some("tick_overdue")
                )
                && row.age_s.is_some_and(|s| s >= threshold_s);
            failing_overdue || stale_overdue
        })
        .collect();
    if overdue.is_empty() {
        crate::operator_notice::forget_at(store, SIGNAL_KEY);
        return WatchOutcome {
            acted: 0,
            skip_reason: Some("clear".to_string()),
            detail: "no arm past threshold".to_string(),
        };
    }
    let mut token_parts: Vec<String> = overdue
        .iter()
        .map(|row| format!("{}@{}", row.arm, anchor(row, now_unix)))
        .collect();
    token_parts.sort();
    let token = token_parts.join(",");
    let title = "control plane: arm failing";
    let body = notice_body(&overdue);
    let verdict = crate::operator_notice::notify_signal_via(
        store,
        now_unix,
        threshold_s,
        SIGNAL_KEY,
        &token,
        title,
        &body,
        Some("fno agents status"),
        || send(title, &body),
    );
    match verdict {
        crate::operator_notice::Verdict::Sent => WatchOutcome {
            acted: 1,
            skip_reason: None,
            detail: short(&format!("notified: {token}")),
        },
        crate::operator_notice::Verdict::Deduped => WatchOutcome {
            acted: 0,
            skip_reason: None,
            detail: "deduped".to_string(),
        },
        crate::operator_notice::Verdict::RateHeld => WatchOutcome {
            acted: 0,
            skip_reason: Some("rate_held".to_string()),
            detail: short(&format!("held: {token}")),
        },
        crate::operator_notice::Verdict::SendFailed => WatchOutcome {
            acted: 0,
            skip_reason: Some("notify_failed".to_string()),
            detail: "send failed; next tick retries".to_string(),
        },
    }
}

/// One body line per overdue arm, then the pointer. Lines stop when the next
/// one would push the body past the 600-char cap; the pointer is kept
/// whatever the truncation cuts.
fn notice_body(overdue: &[&ArmStatus]) -> String {
    const CAP: usize = 600;
    let pointer = "fno agents status";
    let mut body = String::new();
    for row in overdue {
        let line = row_line(row);
        let sep = if body.is_empty() { "" } else { "\n" };
        if body.len() + sep.len() + line.len() + 1 + pointer.len() > CAP {
            break;
        }
        body.push_str(sep);
        body.push_str(&line);
    }
    if body.is_empty() {
        return pointer.to_string();
    }
    body.push('\n');
    body.push_str(pointer);
    body
}

/// The body line for one overdue row: FAIL names the skip reason and how long the arm has been failing; STALE names the cause and the row age.
fn row_line(row: &ArmStatus) -> String {
    if row.failing {
        let skip = row.skip_reason.as_deref().unwrap_or("unknown");
        return match row.failing_for_s {
            Some(s) => format!("{} FAIL {skip} for {s}s", row.arm),
            None => format!("{} FAIL {skip} no_ok_in_journal", row.arm),
        };
    }
    let cause = row.cause.as_deref().unwrap_or("stale");
    let age = row.age_s.unwrap_or(0);
    format!("{} STALE {cause} for {age}s", row.arm)
}

/// The token anchor: `now - failing_for_s` for a failing row (the newest ok run) and the row's `last_ts` for a stale row. Both stay constant while the episode lasts, so a quiet episode dedupes and a set change is a new token. A failing row with no ok run in the journals anchors on the constant 0: its last_ts is the newest FAILED run and advances per interval, which would re-page the same episode every rate floor.
fn anchor(row: &ArmStatus, now_unix: u64) -> u64 {
    if row.failing {
        return match row.failing_for_s {
            Some(s) => now_unix.saturating_sub(s),
            None => 0,
        };
    }
    row.last_ts
        .as_deref()
        .and_then(crate::tick_ledger::parse_rfc3339_unix)
        .unwrap_or(now_unix)
}

/// The tick row's detail cap, like the other arms.
fn short(text: &str) -> String {
    text.chars().take(200).collect()
}

/// The daemon-facing wrapper: due-check plus one-in-flight gate, the machine_watch shape. The whole body runs off-loop; every path ends in exactly one tick row.
pub fn maybe_tick(arm: &Arm, home: AgentsHome) {
    let interval = Duration::from_secs(ARM_WATCH_INTERVAL_S);
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
    let config_cwd = arm.config_cwd.clone();
    tokio::task::spawn_blocking(move || {
        let _gate = crate::daemon::SweepGate(flag);
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let journals = crate::tick_ledger::journals(&home);
        let mut rows = crate::tick_ledger::read_arms(&journals, now_unix);
        let trace = crate::tick_ledger::read_tick_trace(&journals, now_unix);
        crate::tick_ledger::explain_with_trace(
            &mut rows,
            &DaemonFacts::Up {
                uptime_s: u64::MAX,
                drifted: false,
            },
            &trace,
        );
        let threshold = crate::agents_config::notify_arm_failing_after_s(&config_cwd);
        let store = crate::operator_notice::notify_signals_path();
        let outcome = tick_arm_watch(&rows, threshold, &store, now_unix, |title, body| {
            crate::operator_notice::notify_operator(title, body, Some("fno agents status"))
        });
        let journal = crate::loop_runtime::Journal::new_raw(
            home.events_jsonl(),
            crate::daemon::global_events_path(&home),
        );
        crate::tick_ledger::emit_tick(
            &journal,
            "arm_watch",
            crate::tick_ledger::SCHED_DAEMON,
            outcome.acted,
            outcome.skip_reason.as_deref(),
            Some(&outcome.detail),
            interval.as_secs(),
        );
    });
}
