//! One tick row per control-plane arm run, and the readout built from them.
//!
//! Every scheduled arm of the control plane (king wake, watchdog, pr-watch
//! merge dispatch, active backlog, auto-continue, the stop-hook shim) appends
//! one `control_plane_tick` row to the journal it already uses, saying what it
//! did or why it did nothing. The reader folds every journal into one row per
//! arm: last tick, last action, last skip reason, and a stale verdict when the
//! last tick is older than twice the arm's interval. An arm whose journals
//! hold no tick row at all reads UNOBSERVED, never STALE: the absence of a
//! producer receipt cannot say whether a producer exists, and only an
//! observed receipt is a measurement.
//!
//! The row shape is owned here; the Python arms mirror it through
//! `cli/src/fno/control_plane.py` and `cli/src/fno/events/schema.yaml`, and a
//! fixture row in `cli/tests/events/parity_corpus.jsonl` keeps both
//! validators agreeing on it.

use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};

use crate::loop_runtime::Journal;
use crate::paths::AgentsHome;

/// The journal event type every arm writes once per run.
pub const EVENT_TYPE: &str = "control_plane_tick";

/// The launchd scheduler label the pr-watch-hosted arms write in their tick
/// rows; the same string `cli/src/fno/pr_watch/cli.py` emits.
pub const SCHED_LAUNCHD: &str = "launchd:sh.fno.pr-watcher";
/// The daemon scheduler label the in-daemon arms write.
pub const SCHED_DAEMON: &str = "daemon";

/// One known arm and the default interval its staleness is judged against.
/// The row's own `interval_s` field overrides the default, so an operator who
/// tunes an arm's config keeps the verdict honest without touching this table.
/// `0` marks an event-driven arm (the stop-hook shim): it ticks only when a
/// session stops, so staleness does not apply and it never reads red from
/// quiet.
pub struct ArmSpec {
    pub arm: &'static str,
    pub default_interval_s: u64,
    /// Who schedules this arm, so a never-ticked row names its scheduler and
    /// `explain` can blame the tier that owns the silence.
    pub scheduler: &'static str,
}

/// Every arm the readout shows, whether or not it has ever ticked.
pub const KNOWN_ARMS: &[ArmSpec] = &[
    ArmSpec {
        arm: "king_wake",
        default_interval_s: 900,
        scheduler: SCHED_LAUNCHD,
    },
    ArmSpec {
        arm: "watchdog",
        default_interval_s: 600,
        scheduler: SCHED_LAUNCHD,
    },
    ArmSpec {
        arm: "pr_watch_merge",
        default_interval_s: 600,
        scheduler: SCHED_LAUNCHD,
    },
    ArmSpec {
        arm: "active_backlog",
        default_interval_s: 300,
        scheduler: SCHED_DAEMON,
    },
    ArmSpec {
        arm: "auto_continue",
        default_interval_s: 1800,
        scheduler: "session",
    },
    ArmSpec {
        arm: "notify_watch",
        default_interval_s: 300,
        scheduler: SCHED_LAUNCHD,
    },
    ArmSpec {
        arm: "stop_hook",
        default_interval_s: 0,
        scheduler: "hook:target-stop-hook",
    },
    ArmSpec {
        arm: "reap",
        default_interval_s: 60,
        scheduler: SCHED_DAEMON,
    },
    ArmSpec {
        arm: "retire",
        default_interval_s: 300,
        scheduler: SCHED_DAEMON,
    },
    ArmSpec {
        arm: "machine_watch",
        default_interval_s: 300,
        scheduler: SCHED_DAEMON,
    },
    ArmSpec {
        arm: "arm_watch",
        default_interval_s: 300,
        scheduler: SCHED_DAEMON,
    },
    ArmSpec {
        arm: "provider_cap",
        default_interval_s: crate::provider_cap::PROVIDER_CAP_INTERVAL_S,
        scheduler: SCHED_DAEMON,
    },
    ArmSpec {
        arm: "merge_close",
        default_interval_s: crate::merge_close::MERGE_CLOSE_INTERVAL_S,
        scheduler: SCHED_DAEMON,
    },
    ArmSpec {
        arm: "crown_ledger",
        default_interval_s: crate::king_ledger::CROWN_LEDGER_INTERVAL_S,
        scheduler: SCHED_DAEMON,
    },
];

/// Build the `data` object of one tick row. `skip_reason` is a single token
/// (`no_crowned_target`, `watchdog_off`, `env_broken`, ...); `detail` is a
/// short human string.
pub fn tick_data(
    arm: &str,
    scheduler: &str,
    acted: u64,
    skip_reason: Option<&str>,
    detail: Option<&str>,
    interval_s: u64,
) -> Value {
    let mut data = json!({
        "arm": arm,
        "scheduler": scheduler,
        "acted": acted,
        "interval_s": interval_s,
    });
    let obj = data.as_object_mut().expect("literal is an object");
    match skip_reason {
        Some(reason) => obj.insert("skip_reason".into(), Value::String(reason.to_string())),
        None => obj.insert("skip_reason".into(), Value::Null),
    };
    match detail {
        Some(text) => obj.insert("detail".into(), Value::String(text.to_string())),
        None => obj.insert("detail".into(), Value::Null),
    };
    data
}

/// Append one tick row through the loop journal (project journal + global
/// mirror), the journal the daemon arms already use.
pub fn emit_tick(
    journal: &Journal,
    arm: &str,
    scheduler: &str,
    acted: u64,
    skip_reason: Option<&str>,
    detail: Option<&str>,
    interval_s: u64,
) {
    let _ = journal.append(
        EVENT_TYPE,
        tick_data(arm, scheduler, acted, skip_reason, detail, interval_s),
    );
}

/// Whether the scanned journals hold any producer receipt for an arm.
/// `Unobserved` is evidence of nothing: no tick row was found, which cannot
/// say whether a producer exists, started, or stopped before its first tick.
/// It never upgrades to a staleness or failure verdict, and it never feeds
/// cross-arm scheduler inference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProducerEvidence {
    Unobserved,
    Observed,
}

/// One rendered arm row for the readout.
#[derive(Debug, Clone, Serialize)]
pub struct ArmStatus {
    pub arm: String,
    pub scheduler: Option<String>,
    /// RFC3339 ts of the newest tick, or None when the arm never ticked.
    pub last_ts: Option<String>,
    pub age_s: Option<u64>,
    pub acted: Option<u64>,
    pub skip_reason: Option<String>,
    pub detail: Option<String>,
    pub interval_s: u64,
    /// The producer receipt this row was built from: `unobserved` when no
    /// tick row exists, `observed` when one does. The honest runtime claim -
    /// source inspection can find call sites, but an empty journal cannot
    /// prove an emitter's absence.
    pub producer_evidence: ProducerEvidence,
    /// True when the newest tick is older than twice the arm's interval.
    /// Event-driven arms (interval 0) never read stale, and neither does an
    /// unobserved row: absence of a receipt is not a staleness measurement.
    pub stale: bool,
    /// Its newest run failed: the skip reason is itself a failure token
    /// ([`FAILURE_SKIPS`]). Independent of `stale` - the arm broken longest
    /// is the one that most needs its diagnosis. Renders FAIL when fresh;
    /// a stale row keeps the STALE verdict.
    pub failing: bool,
    /// Seconds since the newest run that did NOT fail, set only while
    /// [`ArmStatus::failing`]. `None` while failing means the journal holds
    /// no non-failure run for the arm at all.
    pub failing_for_s: Option<u64>,
    /// Why a stale row is red, set by [`explain`]. `Some("unexplained")`
    /// means the rules ran and found nothing - a written token, never an
    /// absent field, so the reader can tell the rules ran.
    pub cause: Option<String>,
    /// The rendered readout line, filled by [`explain`] for every row, red or
    /// not, so no consumer re-formats it.
    pub line: String,
}

/// The journal list every arms read folds: the agents home journal plus the
/// global mirror derived from the agents root (its parent dir), never a
/// hand-built path. One list for the client readout and the arm_watch daemon
/// arm, so two hand-built lists cannot drift.
pub fn journals(home: &AgentsHome) -> Vec<PathBuf> {
    let global = home
        .root()
        .parent()
        .map(|p| p.join("events.jsonl"))
        .unwrap_or_else(|| home.events_jsonl());
    vec![home.events_jsonl(), global]
}

/// Fold every journal (plus `.1` rotations) into one row per known arm.
/// Unknown arms seen in the journals are appended after the known ones, so a
/// new emitter deploys before its reader does.
pub fn read_arms(journals: &[PathBuf], now_unix: u64) -> Vec<ArmStatus> {
    let mut newest: HashMap<String, NewestTick> = HashMap::new();
    let mut newest_ok: HashMap<String, NewestTick> = HashMap::new();
    let mut paths: Vec<PathBuf> = Vec::new();
    for journal in journals {
        paths.push(journal.clone());
        paths.push(rotation_path(journal));
    }
    for path in &paths {
        scan_journal(path, &mut newest, &mut newest_ok);
    }

    let mut rows: Vec<ArmStatus> = KNOWN_ARMS
        .iter()
        .map(|spec| {
            arm_status(
                spec.arm,
                Some(spec.scheduler),
                spec.default_interval_s,
                newest.get(spec.arm),
                newest_ok.get(spec.arm),
                now_unix,
            )
        })
        .collect();
    let mut extra: Vec<ArmStatus> = newest
        .keys()
        .filter(|arm| !KNOWN_ARMS.iter().any(|spec| spec.arm == *arm))
        .map(|arm| arm_status(arm, None, 0, newest.get(arm), newest_ok.get(arm), now_unix))
        .collect();
    extra.sort_by(|a, b| a.arm.cmp(&b.arm));
    rows.extend(extra);
    rows
}

/// The newest tick row seen for an arm: its parsed ts, the raw ts string, and
/// its data object.
#[derive(Clone)]
struct NewestTick {
    ts_unix: u64,
    ts: String,
    data: Value,
}

fn rotation_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".1");
    PathBuf::from(s)
}

fn scan_journal(
    path: &Path,
    newest: &mut HashMap<String, NewestTick>,
    newest_ok: &mut HashMap<String, NewestTick>,
) {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return,
    };
    for line in std::io::BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some(EVENT_TYPE) {
            continue;
        }
        let Some(data) = value.get("data").and_then(Value::as_object) else {
            continue;
        };
        let Some(arm) = data.get("arm").and_then(Value::as_str) else {
            continue;
        };
        let Some(ts_unix) = value
            .get("ts")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_unix)
        else {
            continue;
        };
        let row = NewestTick {
            ts_unix,
            ts: value
                .get("ts")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            data: Value::Object(data.clone()),
        };
        if fresher_than(newest, arm, ts_unix) {
            newest.insert(arm.to_string(), row.clone());
        }
        // The newest run that did NOT fail anchors `failing_for_s`: how long
        // the arm has been failing, not merely how long since it last spoke.
        let ok = !data
            .get("skip_reason")
            .and_then(Value::as_str)
            .is_some_and(|r| FAILURE_SKIPS.contains(&r));
        if ok && fresher_than(newest_ok, arm, ts_unix) {
            newest_ok.insert(arm.to_string(), row);
        }
    }
}

fn fresher_than(map: &HashMap<String, NewestTick>, arm: &str, ts_unix: u64) -> bool {
    match map.get(arm) {
        Some(seen) => ts_unix >= seen.ts_unix,
        None => true,
    }
}

fn arm_status(
    spec: &str,
    spec_scheduler: Option<&str>,
    default_interval_s: u64,
    newest: Option<&NewestTick>,
    newest_ok: Option<&NewestTick>,
    now_unix: u64,
) -> ArmStatus {
    let Some(tick) = newest else {
        return ArmStatus {
            arm: spec.to_string(),
            scheduler: spec_scheduler.map(|s| s.to_string()),
            last_ts: None,
            age_s: None,
            acted: None,
            skip_reason: Some("never".to_string()),
            detail: None,
            interval_s: default_interval_s,
            producer_evidence: ProducerEvidence::Unobserved,
            stale: false,
            failing: false,
            failing_for_s: None,
            cause: None,
            line: String::new(),
        };
    };
    let interval_s = tick
        .data
        .get("interval_s")
        .and_then(Value::as_u64)
        .unwrap_or(default_interval_s);
    let age_s = now_unix.saturating_sub(tick.ts_unix);
    let stale = interval_s > 0 && age_s > interval_s * 2;
    let skip_reason = str_field(&tick.data, "skip_reason");
    let failing = skip_reason
        .as_deref()
        .is_some_and(|r| FAILURE_SKIPS.contains(&r));
    let failing_for_s = if failing {
        newest_ok.map(|ok| now_unix.saturating_sub(ok.ts_unix))
    } else {
        None
    };
    ArmStatus {
        arm: spec.to_string(),
        scheduler: str_field(&tick.data, "scheduler"),
        last_ts: Some(tick.ts.clone()),
        age_s: Some(age_s),
        acted: tick.data.get("acted").and_then(Value::as_u64),
        skip_reason,
        detail: str_field(&tick.data, "detail"),
        interval_s,
        producer_evidence: ProducerEvidence::Observed,
        stale,
        failing,
        failing_for_s,
        cause: None,
        line: String::new(),
    }
}

fn str_field(data: &Value, field: &str) -> Option<String> {
    data.get(field)
        .and_then(Value::as_str)
        .map(|s| s.to_string())
}

/// The one Rust-owned verdict on whether a row asks the operator for
/// attention: no producer receipt was ever observed, or the newest observed
/// receipt is stale or failed. The status payload publishes its selection as
/// `arms_attention`; consumers print the rows and never re-derive the
/// verdict from the legacy booleans.
pub fn needs_attention(row: &ArmStatus) -> bool {
    row.producer_evidence == ProducerEvidence::Unobserved || row.stale || row.failing
}

/// Skip reasons that mean the arm ran and its run failed - not that it chose
/// to skip. Sources: the pr-watch tick's outcome tokens (disabled, lock_held,
/// quota_skip pass through; timeout/error fail), the king-wake and notify
/// emitters' failure tokens, and auto_continue's `next-error` (a non-zero,
/// malformed or timed-out `backlog next`), `spawn-failed` (the dispatch it
/// fired exited non-zero), and active_backlog's `env_broken` (the resolver
/// shelled out and failed: no usable `fno`, non-zero exit, unreadable
/// receipt -): an arm that could not compute its input, or whose
/// action failed, has not skipped - it has failed. `degraded` is
/// deliberately absent: it is emitted by an arm that ran and acted while one
/// read came back thin, and one transient gh read failure must not turn a
/// fresh row red.
const FAILURE_SKIPS: &[&str] = &[
    "timeout",
    "error",
    "next-error",
    "spawn-failed",
    "env_broken",
    "wake_failed",
    "sweep_failed",
    "notify_failed",
    "registry_unreadable",
];

/// Skip reasons that mean the arm is OFF by configuration. Its silence is the
/// config speaking, not a scheduler that died - no restart helps an arm whose
/// switch is off (the second half of the fleet-faq "one label for two
/// causes" entry this ships alongside `drain_disabled`).
const CONFIGURED_OFF_SKIPS: &[&str] = &[
    "disabled",
    "gate:disabled",
    "drain_disabled",
    "watchdog_off",
    "wake_disabled",
];

/// What the reader holds about the daemon when it explains the rows. Computed
/// once in the client's `run_status` from the status payload it already has.
pub enum DaemonFacts {
    Up { uptime_s: u64, drifted: bool },
    Down,
    Unknown,
}

/// The newest pr_watch tick attempt and end records, folded from the same
/// journals the arm rows come from. A tick attempt newer than the last
/// recorded merge row is a tick that STARTED and left the arm stale - the
/// evidence that separates a completion fault from a scheduler that never
/// fired. Ages are against the same `now_unix` the arm read used.
#[derive(Debug, Serialize, Default)]
pub struct TickTrace {
    pub attempt_ts_unix: Option<u64>,
    pub attempt_age_s: Option<u64>,
    pub end_ts_unix: Option<u64>,
    pub end_age_s: Option<u64>,
    pub end_phase: Option<String>,
    pub end_outcome: Option<String>,
    /// Set when a stale launchd tier's registered plist lives outside the
    /// installer's LaunchAgents path - the 2026-09-08 shape where a pytest
    /// tempdir registration displaced the real pr-watcher.
    pub foreign_plist: Option<String>,
}

/// Fold the newest `pr_watch_tick_attempt` / `pr_watch_tick_end` records out
/// of the journals (plus `.1` rotations). Absent records leave defaults: the
/// trace never invents a tick.
pub fn read_tick_trace(journals: &[PathBuf], now_unix: u64) -> TickTrace {
    let mut paths: Vec<PathBuf> = Vec::new();
    for journal in journals {
        paths.push(journal.clone());
        paths.push(rotation_path(journal));
    }
    let mut trace = TickTrace::default();
    for path in &paths {
        let file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        for line in std::io::BufReader::new(file).lines() {
            let Ok(line) = line else { continue };
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let typ = value.get("type").and_then(Value::as_str).unwrap_or("");
            if typ != "pr_watch_tick_attempt" && typ != "pr_watch_tick_end" {
                continue;
            }
            let Some(ts_unix) = value
                .get("ts")
                .and_then(Value::as_str)
                .and_then(parse_rfc3339_unix)
            else {
                continue;
            };
            let data = value.get("data").cloned().unwrap_or(Value::Null);
            if typ == "pr_watch_tick_attempt" {
                if trace.attempt_ts_unix.is_none_or(|prev| ts_unix >= prev) {
                    trace.attempt_ts_unix = Some(ts_unix);
                    trace.attempt_age_s = Some(now_unix.saturating_sub(ts_unix));
                }
            } else if trace.end_ts_unix.is_none_or(|prev| ts_unix >= prev) {
                trace.end_ts_unix = Some(ts_unix);
                trace.end_age_s = Some(now_unix.saturating_sub(ts_unix));
                trace.end_phase = str_field(&data, "phase");
                trace.end_outcome = str_field(&data, "outcome");
            }
        }
    }
    trace
}

/// The pr-watcher label behind [`SCHED_LAUNCHD`].
fn launchd_label() -> &'static str {
    SCHED_LAUNCHD
        .strip_prefix("launchd:")
        .unwrap_or(SCHED_LAUNCHD)
}

/// The registered plist path from `launchctl print` output, when it is not the
/// one the installer writes. The first `path = ` line is the job's plist; the
/// `stdout path =` / `stderr path =` lines name the job's own log files and do
/// not match the prefix. A textually different path that resolves to the same
/// file (symlinked HOME, alternate volume spelling) is still the installer's
/// own registration. `None` means healthy or unreadable - both leave the
/// existing cause rules in charge.
pub fn foreign_plist_path(print_stdout: &str, home: &Path) -> Option<String> {
    let expected = home
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{}.plist", launchd_label()));
    print_stdout.lines().find_map(|line| {
        let line = line.trim();
        line.strip_prefix("path = ")
            .map(|p| p.trim().to_string())
            .filter(|p| {
                let cand = Path::new(p);
                if cand == expected {
                    return false;
                }
                match (
                    std::fs::canonicalize(cand),
                    std::fs::canonicalize(&expected),
                ) {
                    (Ok(cand), Ok(expected)) => cand != expected,
                    // A nonexistent candidate cannot be the installer's file.
                    _ => true,
                }
            })
    })
}

/// [`read_tick_trace`] plus one live launchd probe: when any launchd-scheduled
/// arm is stale, read the registered plist path so the cause can name a
/// foreign registration instead of a bare tick_overdue. macOS only; bounded to
/// 2s; every failure mode (nonzero exit, kill, absent HOME) leaves the trace
/// untouched. A healthy tier runs no exec at all.
pub fn read_tick_trace_live(journals: &[PathBuf], rows: &[ArmStatus], now_unix: u64) -> TickTrace {
    let mut trace = read_tick_trace(journals, now_unix);
    let stale_launchd = rows
        .iter()
        .any(|r| r.stale && r.scheduler.as_deref() == Some(SCHED_LAUNCHD));
    if !stale_launchd || !cfg!(target_os = "macos") {
        return trace;
    }
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return trace;
    };
    let label = launchd_label();
    // SAFETY: getuid reads a per-process kernel value; it cannot fail or race.
    let uid = unsafe { libc::getuid() };
    let cmd = vec![
        "launchctl".to_string(),
        "print".to_string(),
        format!("gui/{uid}/{label}"),
    ];
    if let Ok(stdout) = crate::king_board::budget::run_with_timeout(
        &cmd,
        Path::new("."),
        std::time::Duration::from_secs(2),
    ) {
        let text = String::from_utf8_lossy(&stdout);
        trace.foreign_plist = foreign_plist_path(&text, &home);
    }
    trace
}

/// The cause token + hint for a stale launchd arm while pr_watch_merge is
/// itself stale. `tick_overdue` is a state, never a cause: the
/// reader measured only that no completed tick stamp landed. The tick records
/// say more when they can: a tick attempt newer than the last
/// recorded merge row is a tick that started and did not complete, and the
/// newest end record names the phase. Genuine silence states itself as "no
/// tick stamp".
fn tick_overdue_cause(pm_last_ts: Option<&str>, trace: Option<&TickTrace>) -> (String, String) {
    // The foreign registration is the most specific fact on the table: a
    // launchd tier can be stale because the job's plist was displaced by a
    // registration from somewhere the installer would never write,
    // and no journal-side rule can see that. Name it before the tick-state
    // rules so the readout points at the actual cause.
    if let Some(p) = trace.and_then(|t| t.foreign_plist.as_deref()) {
        return (
            "launchd_foreign_plist".to_string(),
            format!(
                "sh.fno.pr-watcher is registered from {p}, not the installer's \
                 LaunchAgents path; run fno do pr watch refresh"
            ),
        );
    }
    let pm_ts = pm_last_ts.and_then(parse_rfc3339_unix);
    let tick_ts = trace.and_then(|t| {
        [t.attempt_ts_unix, t.end_ts_unix]
            .into_iter()
            .flatten()
            .max()
    });
    let started_after_pm = match (tick_ts, pm_ts) {
        (Some(tick), Some(pm_ts)) => tick > pm_ts,
        (Some(_), None) => true,
        _ => false,
    };
    if started_after_pm {
        if let Some(t) = trace {
            if let (Some(phase), Some(outcome)) = (t.end_phase.as_deref(), t.end_outcome.as_deref())
            {
                return (
                    "tick_overdue".to_string(),
                    format!(
                        "the tick started and did not complete (phase {phase}, outcome {outcome}); \
                         run fno do pr watch status"
                    ),
                );
            }
            let age = match t.attempt_age_s {
                Some(s) => format!("{s}s ago"),
                None => "recently".to_string(),
            };
            return (
                "tick_overdue".to_string(),
                format!(
                    "a tick started {age} and left no completion record; \
                         run fno do pr watch status"
                ),
            );
        }
    }
    (
        "tick_overdue".to_string(),
        "no tick stamp inside 2x interval; run fno do pr watch status".to_string(),
    )
}

/// Fill `cause` and `line` on every row. A stale row takes the first cause
/// that holds; an observed-stale daemon arm on a young daemon reads
/// `pending` instead of red. An unobserved row is left alone: no measured
/// cause applies to a receipt that does not exist. `unexplained` is written
/// when no rule fires, so a red row names its reason instead of daring the
/// operator to guess whether the arm or its scheduler broke.
pub fn explain(rows: &mut [ArmStatus], daemon: &DaemonFacts) {
    explain_inner(rows, daemon, None)
}

/// [`explain`] with the pr_watch tick trace folded in, so a stale launchd
/// tier can say "the tick started and did not complete" instead of blaming
/// the scheduler.
pub fn explain_with_trace(rows: &mut [ArmStatus], daemon: &DaemonFacts, trace: &TickTrace) {
    explain_inner(rows, daemon, Some(trace))
}

fn explain_inner(rows: &mut [ArmStatus], daemon: &DaemonFacts, trace: Option<&TickTrace>) {
    // Correct a masked merge row BEFORE pm_fresh_failure is read: the
    // tick_timeout rule for the other launchd arms is only as honest as the
    // merge row underneath it.
    let pm_idx = rows.iter().position(|r| r.arm == "pr_watch_merge");
    let mut pm_tick_hint: Option<String> = None;
    if let (Some(i), Some(t)) = (pm_idx, trace) {
        if merge_row_masked_by_tick_end(&rows[i], t) {
            let outcome = t.end_outcome.as_deref().unwrap_or_default();
            let phase = t.end_phase.as_deref().unwrap_or("unknown");
            let pm = &mut rows[i];
            pm.failing = true;
            // The synthesized failure is not an absence: the row itself was
            // the newest healthy run, so it anchors the duration too.
            pm.failing_for_s = pm.age_s;
            pm.cause = Some("tick_timeout".to_string());
            pm_tick_hint = Some(format!(
                "the tick containing this phase ended {outcome} in phase {phase}; \
                 run fno do pr watch status"
            ));
        }
    }
    // The cross-arm flip runs before anything reads `row.stale`, so
    // `pm_stale` below sees the tier's true state instead of a pr_watch_merge
    // row still reading fresh inside its own doubled grace.
    let down = down_schedulers(rows);
    let mut cross_arm = vec![false; rows.len()];
    for (i, row) in rows.iter_mut().enumerate() {
        if !row.stale
            && row.producer_evidence == ProducerEvidence::Observed
            && row.interval_s > 0
            && row.scheduler.as_deref().is_some_and(|s| down.contains(s))
        {
            row.stale = true;
            cross_arm[i] = true;
        }
    }
    let pm = rows.iter().find(|r| r.arm == "pr_watch_merge");
    let pm_fresh_failure = pm.is_some_and(|r| !r.stale && r.failing);
    let pm_stale = pm.is_some_and(|r| r.stale);
    let pm_last_ts = pm.and_then(|r| r.last_ts.clone());
    for (i, row) in rows.iter_mut().enumerate() {
        if row.stale {
            let (cause, hint) = if pm_stale && row.scheduler.as_deref() == Some(SCHED_LAUNCHD) {
                tick_overdue_cause(pm_last_ts.as_deref(), trace)
            } else {
                let cause = stale_cause(row, daemon, pm_fresh_failure).unwrap_or_else(|| {
                    if cross_arm[i] {
                        "scheduler_down"
                    } else {
                        "unexplained"
                    }
                    .to_string()
                });
                let hint = cause_hint(&cause, daemon);
                (cause, hint)
            };
            if cause == "daemon_young" {
                row.stale = false;
            }
            row.cause = Some(cause.clone());
            let mut line = render_row(row);
            line.push_str(&format!(" cause={cause} ({hint})"));
            row.line = line;
        } else {
            let mut line = render_row(row);
            if let Some(hint) = pm_tick_hint.as_deref() {
                if row.cause.as_deref() == Some("tick_timeout") {
                    line.push_str(&format!(" cause=tick_timeout ({hint})"));
                }
            }
            row.line = line;
        }
    }
}

/// Schedulers whose every interval-bearing arm has gone silent together. One
/// arm silent is an arm problem. All of them silent is a job problem, and the
/// row already names the job. A scheduler hosting one interval-bearing arm is
/// skipped: the verdict there would be the per-arm rule under a second name.
/// Unobserved rows keep their seat in the silence vote (their interval still
/// sets the floor) but cannot by themselves establish the verdict: a tier
/// with no observed receipt is evidence of nothing, never a dead scheduler
/// (AC6).
fn down_schedulers(rows: &[ArmStatus]) -> HashSet<String> {
    let mut by_sched: HashMap<&str, Vec<&ArmStatus>> = HashMap::new();
    for row in rows.iter().filter(|r| r.interval_s > 0) {
        if let Some(sched) = row.scheduler.as_deref() {
            by_sched.entry(sched).or_default().push(row);
        }
    }
    by_sched
        .into_iter()
        .filter(|(_, arms)| arms.len() >= 2)
        .filter(|(_, arms)| {
            arms.iter()
                .any(|a| a.producer_evidence == ProducerEvidence::Observed)
        })
        .filter(|(_, arms)| {
            let floor = arms.iter().map(|a| a.interval_s).min().unwrap_or(0) * 2;
            arms.iter().all(|a| match a.age_s {
                None => true,
                Some(age) => age > a.interval_s && age > floor,
            })
        })
        .map(|(sched, _)| sched.to_string())
        .collect()
}

/// The merge phase stamps pr_watch_merge at its own end, so the
/// row's ok describes the merge phase, not the tick or the sweep. When the containing
/// tick's end record landed newer and carries a failure token, the reader
/// corrects the row instead of trusting it.
fn merge_row_masked_by_tick_end(row: &ArmStatus, trace: &TickTrace) -> bool {
    if row.stale || row.failing {
        return false;
    }
    let Some(outcome) = trace.end_outcome.as_deref() else {
        return false;
    };
    if !FAILURE_SKIPS.contains(&outcome) {
        return false;
    }
    let Some(end_ts) = trace.end_ts_unix else {
        return false;
    };
    let Some(row_ts) = row.last_ts.as_deref().and_then(parse_rfc3339_unix) else {
        return false;
    };
    end_ts > row_ts
}

/// The first cause that holds for a stale row, in table order; `None` leaves
/// the row to `unexplained`. A stale launchd tier with a stale
/// pr_watch_merge is handled by the caller: it reads the tick trace and
/// answers `tick_overdue` with evidence, not this table.
fn stale_cause(row: &ArmStatus, daemon: &DaemonFacts, pm_fresh_failure: bool) -> Option<String> {
    // Configured-off outranks every scheduler cause: a restart cannot help an
    // arm whose switch is off, even when the daemon is also down.
    if row
        .skip_reason
        .as_deref()
        .is_some_and(|r| CONFIGURED_OFF_SKIPS.contains(&r))
    {
        return Some("configured_off".to_string());
    }
    let sched = row.scheduler.as_deref();
    if sched == Some(SCHED_DAEMON) {
        if let DaemonFacts::Up { uptime_s, drifted } = *daemon {
            if uptime_s <= row.interval_s * 2 {
                return Some("daemon_young".to_string());
            }
            if drifted {
                return Some("stale_daemon".to_string());
            }
        }
        if matches!(daemon, DaemonFacts::Down) {
            return Some("daemon_down".to_string());
        }
        return None;
    }
    if sched == Some(SCHED_LAUNCHD) && row.arm != "pr_watch_merge" && pm_fresh_failure {
        return Some("tick_timeout".to_string());
    }
    None
}

/// The human hint appended after each cause token.
fn cause_hint(cause: &str, daemon: &DaemonFacts) -> String {
    match cause {
        "daemon_young" => match daemon {
            DaemonFacts::Up { uptime_s, .. } => {
                format!("daemon up {uptime_s}s, first window not elapsed")
            }
            _ => "daemon up, first window not elapsed".to_string(),
        },
        "stale_daemon" => "daemon predates the installed build; run fno agents restart".to_string(),
        "configured_off" => {
            "the arm is off in config; its age is the switch, not a dead scheduler".to_string()
        }
        "daemon_down" => "daemon not running".to_string(),
        "tick_timeout" => {
            "the pr-watch tick broke before this arm ran; see pr_watch_merge".to_string()
        }
        "scheduler_down" => {
            "every arm on this scheduler is silent; the job is not running, the arm is fine"
                .to_string()
        }
        _ => "scheduler looks healthy; the arm itself did not tick".to_string(),
    }
}

/// The per-row readout format, owned here so every consumer prints the same
/// line. Verdict: UNOBSERVED when no producer receipt exists (before every
/// other verdict: absence is not staleness, failure, or ok), STALE when
/// stale, FAIL when failing, pending when the cause is daemon_young, else
/// ok. The `cause=...` suffix is appended by `explain`.
pub fn render_row(row: &ArmStatus) -> String {
    let verdict = if row.producer_evidence == ProducerEvidence::Unobserved {
        "UNOBSERVED"
    } else if row.stale {
        "STALE"
    } else if row.failing {
        "FAIL"
    } else if row.cause.as_deref() == Some("daemon_young") {
        "pending"
    } else {
        "ok"
    };
    let age = match row.age_s {
        Some(s) => format!("{s}s ago"),
        None => "never".to_string(),
    };
    let skip = row
        .skip_reason
        .as_deref()
        .map(|r| format!(" skip={r}"))
        .unwrap_or_default();
    let acted = row.acted.map(|n| format!(" acted={n}")).unwrap_or_default();
    let scheduler = row
        .scheduler
        .as_deref()
        .map(|s| format!(" via={s}"))
        .unwrap_or_default();
    let detail = row
        .detail
        .as_deref()
        .map(|d| format!(" {d}"))
        .unwrap_or_default();
    let failing_for = if row.failing {
        match row.failing_for_s {
            Some(s) => format!(" failing_for={s}s"),
            None => " no_ok_in_journal".to_string(),
        }
    } else {
        String::new()
    };
    format!(
        "{arm:<16} {verdict:<5} {age:>10}{acted}{skip}{scheduler}{detail}{failing_for}",
        arm = row.arm,
        verdict = verdict,
        age = age,
    )
}

/// Parse the two `ts` shapes the journals carry: second precision
/// (`2026-09-04T12:34:56Z`) and millisecond precision
/// (`2026-09-04T12:34:56.789Z`). Returns unix seconds.
pub(crate) fn parse_rfc3339_unix(ts: &str) -> Option<u64> {
    let bytes = ts.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let year = digits(&bytes[0..4])?;
    let month = digits(&bytes[5..7])?;
    let day = digits(&bytes[8..10])?;
    let hour = digits(&bytes[11..13])?;
    let minute = digits(&bytes[14..16])?;
    let second = digits(&bytes[17..19])?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(year as i64, month as u32, day as u32);
    let secs = days * 86_400 + hour as i64 * 3_600 + minute as i64 * 60 + second as i64;
    Some(secs.max(0) as u64)
}

fn digits(bytes: &[u8]) -> Option<u64> {
    let mut value: u64 = 0;
    for b in bytes {
        let digit = (*b as char).to_digit(10)? as u64;
        value = value * 10 + digit;
    }
    Some(value)
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm), matching
/// `events::civil_from_unix` in reverse.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "fno-tick-ledger-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        p
    }

    /// AC8-HP: the readout knows the arm even before its first tick - one
    /// `KNOWN_ARMS` row, daemon scheduler, the 900s beat for merge_close.
    #[test]
    fn arm_watch_is_the_eleventh_known_arm_merge_close_the_thirteenth() {
        assert_eq!(KNOWN_ARMS.len(), 14);
        let spec = KNOWN_ARMS
            .iter()
            .find(|s| s.arm == "arm_watch")
            .expect("arm_watch row");
        assert_eq!(spec.default_interval_s, 300);
        assert_eq!(spec.scheduler, SCHED_DAEMON);
        let cap = KNOWN_ARMS
            .iter()
            .find(|s| s.arm == "provider_cap")
            .expect("provider_cap row");
        assert_eq!(
            cap.default_interval_s,
            crate::provider_cap::PROVIDER_CAP_INTERVAL_S
        );
        assert_eq!(cap.scheduler, SCHED_DAEMON);
        let mc = KNOWN_ARMS
            .iter()
            .find(|s| s.arm == "merge_close")
            .expect("merge_close row");
        assert_eq!(
            mc.default_interval_s,
            crate::merge_close::MERGE_CLOSE_INTERVAL_S
        );
        assert_eq!(mc.scheduler, SCHED_DAEMON);
        let cl = KNOWN_ARMS
            .iter()
            .find(|s| s.arm == "crown_ledger")
            .expect("crown_ledger row");
        assert_eq!(
            cl.default_interval_s,
            crate::king_ledger::CROWN_LEDGER_INTERVAL_S
        );
        assert_eq!(cl.scheduler, SCHED_DAEMON);
    }

    fn write_rows(path: &Path, rows: &[Value]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut text = String::new();
        for row in rows {
            text.push_str(&serde_json::to_string(row).unwrap());
            text.push('\n');
        }
        std::fs::write(path, text).unwrap();
    }

    fn tick_envelope(
        ts: &str,
        arm: &str,
        scheduler: &str,
        acted: u64,
        skip: Value,
        interval_s: u64,
    ) -> Value {
        json!({
            "ts": ts,
            "type": EVENT_TYPE,
            "source": "loop",
            "data": {
                "arm": arm,
                "scheduler": scheduler,
                "acted": acted,
                "skip_reason": skip,
                "detail": null,
                "interval_s": interval_s,
            }
        })
    }

    #[test]
    fn emit_lands_one_row_in_the_journal() {
        let dir = temp_dir();
        let project = dir.join("events.jsonl");
        let journal = Journal::new_raw(project.clone(), dir.join("global.jsonl"));
        emit_tick(
            &journal,
            "active_backlog",
            "daemon",
            2,
            None,
            Some("mission=x"),
            300,
        );

        let text = std::fs::read_to_string(&project).unwrap();
        let lines: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["type"], EVENT_TYPE);
        assert_eq!(lines[0]["data"]["arm"], "active_backlog");
        assert_eq!(lines[0]["data"]["scheduler"], "daemon");
        assert_eq!(lines[0]["data"]["acted"], 2);
        assert_eq!(lines[0]["data"]["skip_reason"], Value::Null);
        assert_eq!(lines[0]["data"]["interval_s"], 300);
        assert!(lines[0]["ts"].as_str().unwrap().ends_with('Z'));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn newest_row_per_arm_wins_across_journals_and_rotation() {
        let dir = temp_dir();
        let a = dir.join("global.jsonl");
        let a_rotated = dir.join("global.jsonl.1");
        let b = dir.join("agents.jsonl");
        write_rows(
            &a_rotated,
            &[tick_envelope(
                "2026-09-04T10:00:00Z",
                "king_wake",
                SCHED_DAEMON,
                0,
                json!("no_crowned_target"),
                900,
            )],
        );
        write_rows(
            &a,
            &[tick_envelope(
                "2026-09-04T11:00:00Z",
                "king_wake",
                SCHED_DAEMON,
                1,
                json!(null),
                900,
            )],
        );
        write_rows(
            &b,
            &[tick_envelope(
                "2026-09-04T11:00:00.500Z",
                "active_backlog",
                SCHED_DAEMON,
                3,
                json!(null),
                300,
            )],
        );

        let now = parse_rfc3339_unix("2026-09-04T11:00:10Z").unwrap();
        let rows = read_arms(&[a, b], now);
        let king = rows.iter().find(|r| r.arm == "king_wake").unwrap();
        assert_eq!(king.acted, Some(1));
        assert_eq!(king.skip_reason, None);
        assert!(!king.stale);
        let ab = rows.iter().find(|r| r.arm == "active_backlog").unwrap();
        assert_eq!(ab.acted, Some(3));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stale_at_twice_the_interval_and_on_row_interval_override() {
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        // watchdog's table default is 600s, so 700s alone would read fresh.
        // The row claims interval 300, and 700 > 2x300 flips it stale: the
        // row's own interval, not the table default, drives the verdict.
        write_rows(
            &journal,
            &[tick_envelope(
                "2026-09-04T10:00:00Z",
                "watchdog",
                SCHED_DAEMON,
                0,
                json!("watchdog_off"),
                300,
            )],
        );
        let now = parse_rfc3339_unix("2026-09-04T10:11:40Z").unwrap(); // 700s later
        let rows = read_arms(&[journal.clone()], now);
        let wd = rows.iter().find(|r| r.arm == "watchdog").unwrap();
        assert_eq!(wd.skip_reason.as_deref(), Some("watchdog_off"));
        assert_eq!(wd.producer_evidence, ProducerEvidence::Observed);
        assert!(
            wd.stale,
            "700s against the row's own 300s interval is stale"
        );

        // 599s against the same row: under 2x300, fresh.
        let now_earlier = parse_rfc3339_unix("2026-09-04T10:09:59Z").unwrap();
        let rows = read_arms(&[journal], now_earlier);
        let wd = rows.iter().find(|r| r.arm == "watchdog").unwrap();
        assert!(!wd.stale);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn never_ticked_arm_is_unobserved_and_every_arm_appears() {
        // AC1-HP: an empty journal is not a staleness measurement. Every
        // known arm - interval-bearing or not - reads UNOBSERVED with
        // last_ts=null, stale=false, failing=false, and no measured cause.
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&journal, "").unwrap();
        let rows = read_arms(&[journal.clone()], 1_800_000_000);
        assert!(rows.len() >= KNOWN_ARMS.len());
        for spec in KNOWN_ARMS {
            let row = rows.iter().find(|r| r.arm == spec.arm).unwrap();
            assert_eq!(
                row.producer_evidence,
                ProducerEvidence::Unobserved,
                "arm {} holds no receipt",
                spec.arm
            );
            assert!(!row.stale, "unobserved arm {} never reads stale", spec.arm);
            assert!(
                !row.failing,
                "unobserved arm {} never reads failing",
                spec.arm
            );
            assert_eq!(row.last_ts, None);
        }
        // `explain` owns every row's `line`; run it before the line asserts.
        let mut rows = rows;
        explain(&mut rows, &DaemonFacts::Unknown);
        for spec in KNOWN_ARMS {
            let row = rows.iter().find(|r| r.arm == spec.arm).unwrap();
            assert!(row.line.contains("UNOBSERVED"), "line: {}", row.line);
            assert!(!row.line.contains("STALE"), "line: {}", row.line);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// AC2-HP: an emitted `acted=0` row is a measurement. A fresh zero-work
    /// tick keeps its skip reason, reads ok (not UNOBSERVED), and lands in
    /// no attention set.
    #[test]
    fn a_real_zero_action_tick_is_observed_not_unobserved() {
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        write_rows(
            &journal,
            &[tick_envelope(
                "2026-09-04T11:59:00Z",
                "watchdog",
                SCHED_LAUNCHD,
                0,
                json!("no_work"),
                600,
            )],
        );
        let now = parse_rfc3339_unix("2026-09-04T12:00:00Z").unwrap();
        let rows = read_arms(&[journal], now);
        let wd = rows.iter().find(|r| r.arm == "watchdog").unwrap();
        assert_eq!(wd.producer_evidence, ProducerEvidence::Observed);
        assert_eq!(wd.skip_reason.as_deref(), Some("no_work"));
        assert!(!wd.stale && !wd.failing);
        assert!(!needs_attention(wd), "line: {}", wd.line);
        let line = render_row(wd);
        assert!(line.contains("acted=0"), "line: {line}");
        assert!(line.contains("skip=no_work"), "line: {line}");
        assert!(!line.contains("UNOBSERVED"), "line: {line}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// AC5: the attention predicate is unobserved, stale, or failing - and
    /// nothing else. A pending (daemon_young) row and an ok row ask for no
    /// operator; a failing row does even while fresh.
    #[test]
    fn needs_attention_covers_exactly_unobserved_stale_and_failing() {
        let observed_fresh_ok = ArmStatus {
            arm: "a".into(),
            scheduler: None,
            last_ts: Some("2026-09-04T12:00:00Z".into()),
            age_s: Some(10),
            acted: Some(1),
            skip_reason: None,
            detail: None,
            interval_s: 300,
            producer_evidence: ProducerEvidence::Observed,
            stale: false,
            failing: false,
            failing_for_s: None,
            cause: None,
            line: String::new(),
        };
        assert!(!needs_attention(&observed_fresh_ok));
        let mut failing = observed_fresh_ok.clone();
        failing.failing = true;
        assert!(needs_attention(&failing));
        let mut stale = observed_fresh_ok.clone();
        stale.stale = true;
        assert!(needs_attention(&stale));
        let mut unobserved = observed_fresh_ok.clone();
        unobserved.producer_evidence = ProducerEvidence::Unobserved;
        assert!(needs_attention(&unobserved));
    }

    /// An empty journal dir: every known arm reads never-ticked.
    fn empty_journal() -> (TempGuard, PathBuf) {
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&journal, "").unwrap();
        (TempGuard(dir), journal)
    }

    /// Removes the temp dir on drop, so explain tests cannot leak state.
    struct TempGuard(PathBuf);
    impl Drop for TempGuard {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    #[test]
    fn configured_off_skip_explains_as_configured_off() {
        // A stale row whose skip_reason says the arm is off in config must
        // explain as configured_off, not as a dead scheduler (AC6).
        let row = ArmStatus {
            arm: "active_backlog".to_string(),
            scheduler: Some("daemon".to_string()),
            last_ts: None,
            age_s: Some(5000),
            acted: Some(0),
            skip_reason: Some("drain_disabled".to_string()),
            detail: None,
            interval_s: 60,
            producer_evidence: ProducerEvidence::Observed,
            stale: true,
            failing: false,
            failing_for_s: None,
            cause: None,
            line: String::new(),
        };
        let cause = stale_cause(&row, &DaemonFacts::Down, false).unwrap();
        assert_eq!(cause, "configured_off");
        assert!(cause_hint(&cause, &DaemonFacts::Down).contains("config"));
    }

    #[test]
    fn env_broken_skip_fails_a_fresh_arm_row() {
        // env_broken means the resolver never produced a reading: the arm
        // could not have acted. That is a failure, not a skip - the
        // verdict must read FAIL and failing_for must age the break.
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        // ok tick 3600s ago, env_broken tick 120s ago (interval 300: fresh).
        write_rows(
            &journal,
            &[
                tick_envelope(
                    "2026-09-04T11:00:00Z",
                    "active_backlog",
                    SCHED_DAEMON,
                    0,
                    json!(null),
                    300,
                ),
                tick_envelope(
                    "2026-09-04T11:58:00Z",
                    "active_backlog",
                    SCHED_DAEMON,
                    0,
                    json!("env_broken"),
                    300,
                ),
            ],
        );
        let now = parse_rfc3339_unix("2026-09-04T12:00:00Z").unwrap();

        let rows = read_arms(&[journal], now);
        let ab = rows.iter().find(|r| r.arm == "active_backlog").unwrap();
        assert!(ab.failing, "env_broken must set failing");
        assert_eq!(ab.failing_for_s, Some(3600));
        let line = render_row(ab);
        assert!(line.contains("FAIL"), "line: {line}");
        assert!(line.contains("skip=env_broken"), "line: {line}");
        assert!(line.contains("failing_for=3600s"), "line: {line}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn degraded_skip_keeps_a_fresh_arm_row_ok() {
        // The regression the doc comment protects: degraded is deliberately
        // absent from FAILURE_SKIPS, so it must not turn a fresh row red.
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        write_rows(
            &journal,
            &[tick_envelope(
                "2026-09-04T11:58:00Z",
                "active_backlog",
                SCHED_DAEMON,
                3,
                json!("degraded"),
                300,
            )],
        );
        let now = parse_rfc3339_unix("2026-09-04T12:00:00Z").unwrap();

        let rows = read_arms(&[journal], now);
        let ab = rows.iter().find(|r| r.arm == "active_backlog").unwrap();
        assert!(!ab.failing, "degraded must not set failing");
        let line = render_row(ab);
        assert!(line.contains(" ok"), "line: {line}");
        assert!(line.contains("skip=degraded"), "line: {line}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn explain_names_a_drifted_daemon_then_falls_through_to_unexplained() {
        // The drifted case runs first: it proves the stale_daemon rule CAN
        // fire before the second call proves the clean-daemon absence. A test
        // that asserted only the absence would pass on a reader that never
        // implemented the rule at all. An observed 3000s-old receipt is what
        // puts the row in the ladder; an unobserved row would read
        // UNOBSERVED instead.
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        write_rows(
            &journal,
            &[tick_envelope(
                "2026-09-04T11:10:00Z",
                "active_backlog",
                SCHED_DAEMON,
                0,
                json!(null),
                300,
            )],
        );
        let now = parse_rfc3339_unix("2026-09-04T12:00:00Z").unwrap();

        let mut rows = read_arms(&[journal.clone()], now);
        explain(
            &mut rows,
            &DaemonFacts::Up {
                uptime_s: 40_000,
                drifted: true,
            },
        );
        let ab = rows.iter().find(|r| r.arm == "active_backlog").unwrap();
        assert_eq!(ab.cause.as_deref(), Some("stale_daemon"));
        assert!(ab.stale);
        assert!(ab.line.contains("STALE"), "line: {}", ab.line);
        assert!(ab.line.contains("fno agents restart"), "line: {}", ab.line);

        let mut rows = read_arms(&[journal], now);
        explain(
            &mut rows,
            &DaemonFacts::Up {
                uptime_s: 40_000,
                drifted: false,
            },
        );
        let ab = rows.iter().find(|r| r.arm == "active_backlog").unwrap();
        assert_eq!(ab.cause.as_deref(), Some("unexplained"));
        assert!(ab.line.contains("STALE"), "line: {}", ab.line);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn explain_pends_a_young_daemon_window_instead_of_red() {
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        // active_backlog last ticked 900s ago (interval 300: overdue at 600).
        write_rows(
            &journal,
            &[tick_envelope(
                "2026-09-04T11:45:00Z",
                "active_backlog",
                SCHED_DAEMON,
                0,
                json!(null),
                300,
            )],
        );
        let now = parse_rfc3339_unix("2026-09-04T12:00:00Z").unwrap();

        let mut rows = read_arms(&[journal], now);
        explain(
            &mut rows,
            &DaemonFacts::Up {
                uptime_s: 120,
                drifted: false,
            },
        );
        let ab = rows.iter().find(|r| r.arm == "active_backlog").unwrap();
        assert!(!ab.stale, "young daemon window clears the stale flag");
        assert_eq!(ab.cause.as_deref(), Some("daemon_young"));
        assert!(ab.line.contains("pending"), "line: {}", ab.line);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn explain_blames_a_timed_out_tick_for_the_arms_after_it() {
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        // pr_watch_merge ticked 100s ago and its tick timed out; king_wake's
        // newest tick is 10000s old (interval 900: stale past 1800).
        write_rows(
            &journal,
            &[
                tick_envelope(
                    "2026-09-04T11:58:20Z",
                    "pr_watch_merge",
                    SCHED_LAUNCHD,
                    0,
                    json!("timeout"),
                    600,
                ),
                tick_envelope(
                    "2026-09-04T09:33:20Z",
                    "king_wake",
                    SCHED_LAUNCHD,
                    0,
                    json!(null),
                    900,
                ),
            ],
        );
        let now = parse_rfc3339_unix("2026-09-04T12:00:00Z").unwrap();

        let mut rows = read_arms(&[journal], now);
        explain(&mut rows, &DaemonFacts::Unknown);
        let pm = rows.iter().find(|r| r.arm == "pr_watch_merge").unwrap();
        assert!(pm.failing, "a fresh timeout row is failing");
        assert!(!pm.stale);
        assert!(pm.line.contains("FAIL"), "line: {}", pm.line);
        let kw = rows.iter().find(|r| r.arm == "king_wake").unwrap();
        assert_eq!(kw.cause.as_deref(), Some("tick_timeout"));
        assert!(kw.line.contains("STALE"), "line: {}", kw.line);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_fresh_next_error_fails_the_auto_continue_arm() {
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        write_rows(
            &journal,
            &[tick_envelope(
                "2026-09-04T11:58:20Z",
                "auto_continue",
                "session",
                0,
                json!("next-error"),
                1800,
            )],
        );
        let now = parse_rfc3339_unix("2026-09-04T12:00:00Z").unwrap();

        let mut rows = read_arms(&[journal], now);
        explain(&mut rows, &DaemonFacts::Unknown);
        let ac = rows.iter().find(|r| r.arm == "auto_continue").unwrap();
        assert!(!ac.stale, "the tick is 100s into a 1800s interval");
        assert!(ac.failing, "an unreadable selection is a failed run");
        assert!(ac.line.contains("FAIL"), "line: {}", ac.line);
        assert!(ac.line.contains("skip=next-error"), "line: {}", ac.line);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_stale_timeout_row_stays_failing_and_names_its_skip_reason() {
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        // king_wake timed out once and never came back: age 1801s against a
        // 900s interval reads stale, and the reason it stopped is still a
        // failure the row must name.
        write_rows(
            &journal,
            &[tick_envelope(
                "2026-09-04T09:33:19Z",
                "king_wake",
                SCHED_LAUNCHD,
                0,
                json!("timeout"),
                900,
            )],
        );
        let now = parse_rfc3339_unix("2026-09-04T10:03:20Z").unwrap();

        let mut rows = read_arms(&[journal], now);
        explain(&mut rows, &DaemonFacts::Unknown);
        let kw = rows.iter().find(|r| r.arm == "king_wake").unwrap();
        assert!(kw.stale, "1801s against 2x900 must read stale");
        assert!(kw.failing, "the newest run is a timeout, stale or not");
        assert!(kw.line.contains("STALE"), "line: {}", kw.line);
        assert!(kw.line.contains("skip=timeout"), "line: {}", kw.line);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn failing_for_s_counts_from_the_last_run_that_did_not_fail() {
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        // ok at 10:00, timeouts at 10:15 and 10:30, read at 10:31:40: the
        // arm has been failing 1900s, not 100s since its newest word.
        write_rows(
            &journal,
            &[
                tick_envelope(
                    "2026-09-04T10:00:00Z",
                    "king_wake",
                    SCHED_LAUNCHD,
                    1,
                    json!(null),
                    900,
                ),
                tick_envelope(
                    "2026-09-04T10:15:00Z",
                    "king_wake",
                    SCHED_LAUNCHD,
                    0,
                    json!("timeout"),
                    900,
                ),
                tick_envelope(
                    "2026-09-04T10:30:00Z",
                    "king_wake",
                    SCHED_LAUNCHD,
                    0,
                    json!("timeout"),
                    900,
                ),
            ],
        );
        let now = parse_rfc3339_unix("2026-09-04T10:31:40Z").unwrap();

        let mut rows = read_arms(&[journal], now);
        explain(&mut rows, &DaemonFacts::Unknown);
        let kw = rows.iter().find(|r| r.arm == "king_wake").unwrap();
        assert!(kw.failing);
        assert_eq!(kw.failing_for_s, Some(1900));
        assert!(kw.line.contains("failing_for=1900s"), "line: {}", kw.line);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_failure_with_no_healthy_run_in_the_journal_says_so() {
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        write_rows(
            &journal,
            &[tick_envelope(
                "2026-09-04T11:58:20Z",
                "auto_continue",
                "session",
                0,
                json!("error"),
                1800,
            )],
        );
        let now = parse_rfc3339_unix("2026-09-04T12:00:00Z").unwrap();

        let mut rows = read_arms(&[journal], now);
        explain(&mut rows, &DaemonFacts::Unknown);
        let ac = rows.iter().find(|r| r.arm == "auto_continue").unwrap();
        assert!(ac.failing);
        assert_eq!(
            ac.failing_for_s, None,
            "no non-failure run anchors the count"
        );
        assert!(ac.line.contains("no_ok_in_journal"), "line: {}", ac.line);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_fresh_spawn_failed_fails_the_auto_continue_arm() {
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        write_rows(
            &journal,
            &[tick_envelope(
                "2026-09-04T11:58:20Z",
                "auto_continue",
                "session",
                0,
                json!("spawn-failed"),
                1800,
            )],
        );
        let now = parse_rfc3339_unix("2026-09-04T12:00:00Z").unwrap();

        let mut rows = read_arms(&[journal], now);
        explain(&mut rows, &DaemonFacts::Unknown);
        let ac = rows.iter().find(|r| r.arm == "auto_continue").unwrap();
        assert!(!ac.stale, "the tick is 100s into a 1800s interval");
        assert!(
            ac.failing,
            "a dispatch that exited non-zero is a failed run"
        );
        assert!(ac.line.contains("FAIL"), "line: {}", ac.line);
        assert!(ac.line.contains("skip=spawn-failed"), "line: {}", ac.line);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fresh_benign_auto_continue_skips_stay_ok() {
        for skip in ["disabled", "no-work"] {
            let dir = temp_dir();
            let journal = dir.join("global.jsonl");
            write_rows(
                &journal,
                &[tick_envelope(
                    "2026-09-04T11:58:20Z",
                    "auto_continue",
                    "session",
                    0,
                    json!(skip),
                    1800,
                )],
            );
            let now = parse_rfc3339_unix("2026-09-04T12:00:00Z").unwrap();

            let mut rows = read_arms(&[journal], now);
            explain(&mut rows, &DaemonFacts::Unknown);
            let ac = rows.iter().find(|r| r.arm == "auto_continue").unwrap();
            assert!(!ac.failing, "{skip} is a choice, not a failure");
            assert!(!ac.line.contains("FAIL"), "line: {}", ac.line);
            assert!(ac.line.contains("ok"), "line: {}", ac.line);
            std::fs::remove_dir_all(&dir).ok();
        }
    }

    #[test]
    fn explain_names_a_down_daemon_for_daemon_arms() {
        // Observed-stale receipts put the arms in the cause ladder at all;
        // an unobserved row would never enter it.
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        write_rows(
            &journal,
            &[
                tick_envelope(
                    "2026-09-04T11:00:00Z",
                    "reap",
                    SCHED_DAEMON,
                    0,
                    json!(null),
                    60,
                ),
                tick_envelope(
                    "2026-09-04T11:00:00Z",
                    "king_wake",
                    SCHED_LAUNCHD,
                    0,
                    json!(null),
                    900,
                ),
                tick_envelope(
                    "2026-09-04T11:00:00Z",
                    "pr_watch_merge",
                    SCHED_LAUNCHD,
                    0,
                    json!(null),
                    600,
                ),
            ],
        );
        let mut rows = read_arms(
            &[journal],
            parse_rfc3339_unix("2026-09-04T12:00:00Z").unwrap(),
        );
        explain(&mut rows, &DaemonFacts::Down);
        let reap = rows.iter().find(|r| r.arm == "reap").unwrap();
        assert_eq!(reap.producer_evidence, ProducerEvidence::Observed);
        assert_eq!(reap.cause.as_deref(), Some("daemon_down"));
        assert!(reap.line.contains("STALE"), "line: {}", reap.line);
        // A launchd arm names its overdue tick tier, not the daemon:
        // pr_watch_merge is itself stale, so the stamps are tier-wide overdue.
        let kw = rows.iter().find(|r| r.arm == "king_wake").unwrap();
        assert_eq!(kw.cause.as_deref(), Some("tick_overdue"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn explain_blames_an_errored_tick_like_a_timed_out_one() {
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        // pr_watch_merge ticked fresh but its run errored; king_wake is stale.
        // Any failure-token skip (not just timeout) blames the tick.
        write_rows(
            &journal,
            &[
                tick_envelope(
                    "2026-09-04T11:58:20Z",
                    "pr_watch_merge",
                    SCHED_LAUNCHD,
                    0,
                    json!("error"),
                    600,
                ),
                tick_envelope(
                    "2026-09-04T09:33:20Z",
                    "king_wake",
                    SCHED_LAUNCHD,
                    0,
                    json!(null),
                    900,
                ),
            ],
        );
        let now = parse_rfc3339_unix("2026-09-04T12:00:00Z").unwrap();

        let mut rows = read_arms(&[journal], now);
        explain(&mut rows, &DaemonFacts::Unknown);
        let kw = rows.iter().find(|r| r.arm == "king_wake").unwrap();
        assert_eq!(kw.cause.as_deref(), Some("tick_timeout"));
        assert!(
            kw.line.contains("the pr-watch tick broke"),
            "line: {}",
            kw.line
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn explain_corrects_a_merge_ok_row_that_its_tick_ended_timeout() {
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        // The sweep finished inside its cap and stamped the merge row ok; the
        // tick then hit its wall in a later phase. The row's ok describes the
        // phase, and the reader corrects it from the tick's end record.
        write_rows(
            &journal,
            &[
                tick_envelope(
                    "2026-09-04T11:58:20Z",
                    "pr_watch_merge",
                    SCHED_LAUNCHD,
                    3,
                    json!(null),
                    600,
                ),
                tick_envelope(
                    "2026-09-04T09:33:20Z",
                    "king_wake",
                    SCHED_LAUNCHD,
                    0,
                    json!(null),
                    900,
                ),
            ],
        );
        let now = parse_rfc3339_unix("2026-09-04T12:00:00Z").unwrap();
        let trace = TickTrace {
            end_ts_unix: Some(parse_rfc3339_unix("2026-09-04T11:59:30Z").unwrap()),
            end_phase: Some("catchup".to_string()),
            end_outcome: Some("timeout".to_string()),
            ..TickTrace::default()
        };

        let mut rows = read_arms(&[journal], now);
        explain_with_trace(&mut rows, &DaemonFacts::Unknown, &trace);
        let pm = rows.iter().find(|r| r.arm == "pr_watch_merge").unwrap();
        assert!(
            pm.failing,
            "the containing tick's timeout outranks the sweep's ok"
        );
        assert_eq!(pm.cause.as_deref(), Some("tick_timeout"));
        assert!(pm.line.contains("FAIL"), "line: {}", pm.line);
        assert!(
            pm.line.contains("ended timeout in phase catchup"),
            "line: {}",
            pm.line
        );
        // The synthesized failure is not an absence: the corrected row itself
        // was the newest healthy run, so the duration anchors to it instead
        // of claiming the journal held no healthy run.
        assert_eq!(pm.failing_for_s, Some(100));
        assert!(
            pm.line.contains("failing_for=100s") && !pm.line.contains("no_ok_in_journal"),
            "line: {}",
            pm.line
        );
        // The downstream rule now sees an honest merge row: the stale arm
        // blames the tick instead of reading unexplained.
        let kw = rows.iter().find(|r| r.arm == "king_wake").unwrap();
        assert_eq!(kw.cause.as_deref(), Some("tick_timeout"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn explain_keeps_a_merge_ok_row_when_the_tick_end_predates_it() {
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        // An end record older than the row is the PREVIOUS tick's outcome;
        // this row's own tick has not ended, so the fresh ok stands.
        write_rows(
            &journal,
            &[tick_envelope(
                "2026-09-04T11:58:20Z",
                "pr_watch_merge",
                SCHED_LAUNCHD,
                3,
                json!(null),
                600,
            )],
        );
        let now = parse_rfc3339_unix("2026-09-04T12:00:00Z").unwrap();
        let trace = TickTrace {
            end_ts_unix: Some(parse_rfc3339_unix("2026-09-04T11:50:00Z").unwrap()),
            end_phase: Some("catchup".to_string()),
            end_outcome: Some("timeout".to_string()),
            ..TickTrace::default()
        };

        let mut rows = read_arms(&[journal], now);
        explain_with_trace(&mut rows, &DaemonFacts::Unknown, &trace);
        let pm = rows.iter().find(|r| r.arm == "pr_watch_merge").unwrap();
        assert!(!pm.failing, "line: {}", pm.line);
        assert_eq!(pm.cause, None);
        assert!(pm.line.contains("ok"), "line: {}", pm.line);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_all_unobserved_tier_establishes_no_measured_cause() {
        let (guard, journal) = empty_journal();
        // AC6-EDGE: every launchd arm unobserved. No receipt exists, so no
        // cross-arm scheduler verdict and no tick_overdue may be derived:
        // absence is not a measurement. Each row keeps UNOBSERVED and no
        // cause, stating the absence as the fact it is.
        let mut rows = read_arms(&[journal], 1_800_000_000);
        let trace = TickTrace::default();
        explain_with_trace(
            &mut rows,
            &DaemonFacts::Up {
                uptime_s: 40_000,
                drifted: false,
            },
            &trace,
        );
        for arm in ["king_wake", "watchdog", "pr_watch_merge", "notify_watch"] {
            let row = rows.iter().find(|r| r.arm == arm).unwrap();
            assert_eq!(
                row.producer_evidence,
                ProducerEvidence::Unobserved,
                "{arm} holds no receipt"
            );
            assert!(!row.stale, "{arm} must not read STALE, line: {}", row.line);
            assert_eq!(row.cause, None, "no measured cause without a receipt");
            assert!(row.line.contains("UNOBSERVED"), "line: {}", row.line);
        }
        drop(guard);
    }

    #[test]
    fn explain_says_the_tick_started_and_did_not_complete_and_names_the_phase() {
        // The fault shape: launchd showed the job loaded and a tick
        // was running throughout, yet pr_watch_merge is stale because ticks
        // died before writing a merge row. The attempt + end records are the
        // evidence; the cause must name the phase, not blame the scheduler.
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        write_rows(
            &journal,
            &[
                tick_envelope(
                    "2026-09-04T11:30:00Z",
                    "pr_watch_merge",
                    SCHED_LAUNCHD,
                    0,
                    json!(null),
                    600,
                ),
                tick_envelope(
                    "2026-09-04T09:33:20Z",
                    "king_wake",
                    SCHED_LAUNCHD,
                    0,
                    json!(null),
                    900,
                ),
                json!({
                    "ts": "2026-09-04T11:58:00Z",
                    "type": "pr_watch_tick_attempt",
                    "source": "daemon",
                    "data": {"pid": 32078, "phase": "entry"},
                }),
                json!({
                    "ts": "2026-09-04T11:59:00Z",
                    "type": "pr_watch_tick_end",
                    "source": "daemon",
                    "data": {"outcome": "error", "why": "self_killed",
                             "phase": "catchup", "duration_s": 60.0, "pid": 32078},
                }),
            ],
        );
        let now = parse_rfc3339_unix("2026-09-04T12:00:00Z").unwrap();

        let mut rows = read_arms(&[journal], now);
        let trace = read_tick_trace(&[dir.join("global.jsonl")], now);
        explain_with_trace(&mut rows, &DaemonFacts::Unknown, &trace);
        let kw = rows.iter().find(|r| r.arm == "king_wake").unwrap();
        assert_eq!(kw.cause.as_deref(), Some("tick_overdue"));
        assert!(
            kw.line.contains("the tick started and did not complete"),
            "line: {}",
            kw.line
        );
        assert!(kw.line.contains("phase catchup"), "line: {}", kw.line);
        // The CAUSE blames no tier: no "silent" wording survives. The row's
        // own `via=` scheduler label is fact, not blame, and stays.
        assert!(!kw.line.contains("silent"), "line: {}", kw.line);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_foreign_registration_names_the_path_and_the_refresh_hint() {
        // The fault shape: the registered plist lives under a pytest
        // tempdir instead of the installer's LaunchAgents path. The cause must
        // name the foreign path and the refresh command, not a bare
        // tick_overdue that reads as "stale install".
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        write_rows(
            &journal,
            &[
                tick_envelope(
                    "2026-09-04T11:30:00Z",
                    "pr_watch_merge",
                    SCHED_LAUNCHD,
                    3,
                    json!(null),
                    600,
                ),
                tick_envelope(
                    "2026-09-04T09:33:20Z",
                    "king_wake",
                    SCHED_LAUNCHD,
                    0,
                    json!(null),
                    900,
                ),
            ],
        );
        let now = parse_rfc3339_unix("2026-09-04T12:00:00Z").unwrap();
        let trace = TickTrace {
            foreign_plist: Some(
                "/private/var/folders/ch/.../T/pytest-of-bb16/pytest-142/\
                 test_ac3hp_install_writes_file0/LaunchAgents/sh.fno.pr-watcher.plist"
                    .to_string(),
            ),
            ..TickTrace::default()
        };

        let mut rows = read_arms(&[journal], now);
        explain_with_trace(&mut rows, &DaemonFacts::Unknown, &trace);
        for arm in ["king_wake", "pr_watch_merge"] {
            let row = rows.iter().find(|r| r.arm == arm).unwrap();
            assert_eq!(
                row.cause.as_deref(),
                Some("launchd_foreign_plist"),
                "{arm}: line {}",
                row.line
            );
            assert!(
                row.line.contains("pytest-142"),
                "{arm} must name the foreign path, line: {}",
                row.line
            );
            assert!(
                row.line.contains("fno do pr watch refresh"),
                "{arm} must name the remedy, line: {}",
                row.line
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_healthy_registration_keeps_tick_overdue() {
        // The control: when the registered path IS the installer's, the trace
        // holds no foreign plist and the stale rows keep the existing
        // tick_overdue cause; an empty print output reads the same.
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        write_rows(
            &journal,
            &[tick_envelope(
                "2026-09-04T11:30:00Z",
                "pr_watch_merge",
                SCHED_LAUNCHD,
                3,
                json!(null),
                600,
            )],
        );
        let now = parse_rfc3339_unix("2026-09-04T12:00:00Z").unwrap();
        let home = std::env::temp_dir();
        let expected = home
            .join("Library")
            .join("LaunchAgents")
            .join("sh.fno.pr-watcher.plist");
        let healthy = format!("\tpath = {}\n", expected.display());
        assert_eq!(foreign_plist_path(&healthy, &home), None);
        assert_eq!(foreign_plist_path("", &home), None);

        let mut rows = read_arms(&[journal], now);
        let trace = TickTrace::default();
        explain_with_trace(&mut rows, &DaemonFacts::Unknown, &trace);
        let pm = rows.iter().find(|r| r.arm == "pr_watch_merge").unwrap();
        assert_eq!(pm.cause.as_deref(), Some("tick_overdue"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn foreign_plist_path_parses_launchctl_print_output() {
        // stdout path / stderr path name the job's log files, not the plist;
        // the first bare `path = ` line wins. Empty output reads healthy.
        let home = std::env::temp_dir();
        let foreign = "/private/var/folders/ch/T/pytest-of-bb16/pytest-142/\
                       test_ac3hp_install_writes_file0/LaunchAgents/sh.fno.pr-watcher.plist";
        let out = format!(
            "\tfirst exit code = 78\n\tpid = 0\n\tstdout path = /tmp/x/y.out\n\
             \tstderr path = /tmp/x/y.err\n\tpath = {foreign}\n\tstate = not running\n"
        );
        assert_eq!(foreign_plist_path(&out, &home), Some(foreign.to_string()));
        // A healthy first line short-circuits: None even with later noise.
        let healthy_first = format!(
            "\tpath = {}\n\tstdout path = /tmp/x/y.out\n",
            home.join("Library/LaunchAgents/sh.fno.pr-watcher.plist")
                .display()
        );
        assert_eq!(foreign_plist_path(&healthy_first, &home), None);
    }

    #[test]
    fn a_textually_different_but_same_file_path_is_not_foreign() {
        // A symlinked HOME spells the installer's path two ways; the file
        // identity, not the spelling, decides. The healthy textual match
        // short-circuits before any syscall in the common case.
        let dir = temp_dir();
        let home = dir.join("home");
        let expected = home
            .join("Library")
            .join("LaunchAgents")
            .join("sh.fno.pr-watcher.plist");
        std::fs::create_dir_all(expected.parent().unwrap()).ok();
        std::fs::write(&expected, "plist").ok();
        let alt = dir.join("homelink");
        std::os::unix::fs::symlink(&home, &alt).ok();
        let registered = format!(
            "\tpath = {}/Library/LaunchAgents/sh.fno.pr-watcher.plist\n",
            alt.display()
        );
        assert_eq!(foreign_plist_path(&registered, &home), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The measured 2026-09-11 outage: the pr-watcher job unloaded at
    /// 10:41Z and at 10:59Z the four launchd arms read 1114s, 1084s, 1123s
    /// and 1113s against intervals 900, 600, 600 and 300. Three of the four
    /// pass the per-arm rule; the cross-arm verdict must still red them all.
    #[test]
    fn cross_arm_flip_reds_the_whole_launchd_tier_in_the_measured_outage() {
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        write_rows(
            &journal,
            &[
                tick_envelope(
                    "2026-09-11T10:40:26Z",
                    "king_wake",
                    SCHED_LAUNCHD,
                    0,
                    json!(null),
                    900,
                ),
                tick_envelope(
                    "2026-09-11T10:40:56Z",
                    "watchdog",
                    SCHED_LAUNCHD,
                    0,
                    json!(null),
                    600,
                ),
                tick_envelope(
                    "2026-09-11T10:40:17Z",
                    "pr_watch_merge",
                    SCHED_LAUNCHD,
                    0,
                    json!(null),
                    600,
                ),
                tick_envelope(
                    "2026-09-11T10:40:27Z",
                    "notify_watch",
                    SCHED_LAUNCHD,
                    0,
                    json!(null),
                    300,
                ),
            ],
        );
        let now = parse_rfc3339_unix("2026-09-11T10:59:00Z").unwrap();

        let mut rows = read_arms(&[journal], now);
        explain(
            &mut rows,
            &DaemonFacts::Up {
                uptime_s: 40_000,
                drifted: false,
            },
        );
        for arm in ["king_wake", "watchdog", "pr_watch_merge", "notify_watch"] {
            let row = rows.iter().find(|r| r.arm == arm).unwrap();
            assert!(row.stale, "{arm} must read STALE, line: {}", row.line);
            assert!(row.line.contains("STALE"), "line: {}", row.line);
            assert!(
                row.cause.as_deref().is_some_and(|c| !c.is_empty()),
                "{arm} has no cause"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn one_silent_arm_stays_an_arm_problem() {
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        write_rows(
            &journal,
            &[
                tick_envelope(
                    "2026-09-11T09:52:20Z",
                    "notify_watch",
                    SCHED_LAUNCHD,
                    0,
                    json!(null),
                    300,
                ),
                tick_envelope(
                    "2026-09-11T10:57:20Z",
                    "king_wake",
                    SCHED_LAUNCHD,
                    0,
                    json!(null),
                    900,
                ),
                tick_envelope(
                    "2026-09-11T10:57:20Z",
                    "watchdog",
                    SCHED_LAUNCHD,
                    0,
                    json!(null),
                    600,
                ),
                tick_envelope(
                    "2026-09-11T10:57:20Z",
                    "pr_watch_merge",
                    SCHED_LAUNCHD,
                    0,
                    json!(null),
                    600,
                ),
            ],
        );
        let now = parse_rfc3339_unix("2026-09-11T10:59:00Z").unwrap();

        let mut rows = read_arms(&[journal], now);
        explain(&mut rows, &DaemonFacts::Unknown);
        let nw = rows.iter().find(|r| r.arm == "notify_watch").unwrap();
        assert!(nw.stale, "notify_watch 4000s against 300 is stale");
        for arm in ["king_wake", "watchdog", "pr_watch_merge"] {
            let row = rows.iter().find(|r| r.arm == arm).unwrap();
            assert!(!row.stale, "{arm} ticked 100s ago and reads ok");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn single_arm_scheduler_is_not_a_second_name_for_the_per_arm_rule() {
        // auto_continue is the only interval-bearing arm on the `session`
        // scheduler. Its silence is judged by its own rule alone - and an
        // empty journal is no silence measurement at all: the row stays
        // UNOBSERVED, which is attention without an invented cause.
        let (guard, journal) = empty_journal();
        let mut rows = read_arms(&[journal], 1_800_000_000);
        explain(&mut rows, &DaemonFacts::Unknown);
        let ac = rows.iter().find(|r| r.arm == "auto_continue").unwrap();
        assert_eq!(ac.producer_evidence, ProducerEvidence::Unobserved);
        assert!(!ac.stale, "unobserved auto_continue never reads stale");
        assert_eq!(ac.cause, None);
        drop(guard);
    }

    #[test]
    fn interval_zero_arm_is_neither_counted_nor_flipped() {
        let (guard, journal) = empty_journal();
        let mut rows = read_arms(&[journal], 1_800_000_000);
        explain(&mut rows, &DaemonFacts::Unknown);
        let sh = rows.iter().find(|r| r.arm == "stop_hook").unwrap();
        assert_eq!(sh.producer_evidence, ProducerEvidence::Unobserved);
        assert!(!sh.stale, "event-driven arm never reads red from quiet");
        assert_eq!(sh.cause, None, "stop_hook is never explained");
        drop(guard);
    }

    #[test]
    fn flipped_rows_that_reach_no_specific_cause_read_scheduler_down() {
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        // Daemon tier: reap is stale by its own rule (500s > 2x60);
        // active_backlog and retire are fresh-but-silent (400s: past their
        // own 300s run, inside 2x). All three silent together = the daemon
        // job stopped, so the two fresh rows flip and take the new cause.
        write_rows(
            &journal,
            &[
                tick_envelope(
                    "2026-09-11T11:51:40Z",
                    "reap",
                    SCHED_DAEMON,
                    0,
                    json!(null),
                    60,
                ),
                tick_envelope(
                    "2026-09-11T11:53:20Z",
                    "active_backlog",
                    SCHED_DAEMON,
                    0,
                    json!(null),
                    300,
                ),
                tick_envelope(
                    "2026-09-11T11:53:20Z",
                    "retire",
                    SCHED_DAEMON,
                    0,
                    json!(null),
                    300,
                ),
                tick_envelope(
                    "2026-09-11T11:58:20Z",
                    "pr_watch_merge",
                    SCHED_LAUNCHD,
                    0,
                    json!(null),
                    600,
                ),
                tick_envelope(
                    "2026-09-11T11:58:20Z",
                    "king_wake",
                    SCHED_LAUNCHD,
                    0,
                    json!(null),
                    900,
                ),
            ],
        );
        let now = parse_rfc3339_unix("2026-09-11T12:00:00Z").unwrap();

        let mut rows = read_arms(&[journal], now);
        explain(
            &mut rows,
            &DaemonFacts::Up {
                uptime_s: 40_000,
                drifted: false,
            },
        );
        for arm in ["active_backlog", "retire"] {
            let row = rows.iter().find(|r| r.arm == arm).unwrap();
            assert_eq!(
                row.cause.as_deref(),
                Some("scheduler_down"),
                "line: {}",
                row.line
            );
            assert!(
                row.line.contains("the job is not running, the arm is fine"),
                "line: {}",
                row.line
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_cross_arm_flip_never_outranks_the_tick_trace_evidence() {
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        write_rows(
            &journal,
            &[
                tick_envelope(
                    "2026-09-11T10:40:26Z",
                    "king_wake",
                    SCHED_LAUNCHD,
                    0,
                    json!(null),
                    900,
                ),
                tick_envelope(
                    "2026-09-11T10:40:17Z",
                    "pr_watch_merge",
                    SCHED_LAUNCHD,
                    0,
                    json!(null),
                    600,
                ),
                json!({
                    "ts": "2026-09-11T10:58:00Z",
                    "type": "pr_watch_tick_attempt",
                    "source": "daemon",
                    "data": {"pid": 32078, "phase": "entry"},
                }),
                json!({
                    "ts": "2026-09-11T10:58:30Z",
                    "type": "pr_watch_tick_end",
                    "source": "daemon",
                    "data": {"outcome": "error", "why": "self_killed",
                             "phase": "catchup", "duration_s": 30.0, "pid": 32078},
                }),
            ],
        );
        let now = parse_rfc3339_unix("2026-09-11T10:59:00Z").unwrap();

        let mut rows = read_arms(&[journal], now);
        let trace = read_tick_trace(&[dir.join("global.jsonl")], now);
        explain_with_trace(&mut rows, &DaemonFacts::Unknown, &trace);
        let kw = rows.iter().find(|r| r.arm == "king_wake").unwrap();
        assert!(kw.stale, "line: {}", kw.line);
        assert_eq!(kw.cause.as_deref(), Some("tick_overdue"));
        assert!(
            kw.line.contains("the tick started and did not complete"),
            "line: {}",
            kw.line
        );
        assert_ne!(kw.cause.as_deref(), Some("scheduler_down"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_young_daemon_un_flips_its_arms_not_scheduler_down() {
        // Three observed-stale daemon arms (900s against intervals 60/300),
        // but the daemon is up 100s - inside reap's first window (2x60).
        // daemon_young un-flips them, and the cross-arm verdict must not
        // survive it. Observed receipts are what make this a staleness
        // question at all; an unobserved row would never enter the ladder.
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        write_rows(
            &journal,
            &[
                tick_envelope(
                    "2026-09-11T11:45:00Z",
                    "reap",
                    SCHED_DAEMON,
                    0,
                    json!(null),
                    60,
                ),
                tick_envelope(
                    "2026-09-11T11:45:00Z",
                    "active_backlog",
                    SCHED_DAEMON,
                    0,
                    json!(null),
                    300,
                ),
                tick_envelope(
                    "2026-09-11T11:45:00Z",
                    "retire",
                    SCHED_DAEMON,
                    0,
                    json!(null),
                    300,
                ),
            ],
        );
        let mut rows = read_arms(
            &[journal],
            parse_rfc3339_unix("2026-09-11T12:00:00Z").unwrap(),
        );
        explain(
            &mut rows,
            &DaemonFacts::Up {
                uptime_s: 100,
                drifted: false,
            },
        );
        for arm in ["active_backlog", "reap", "retire"] {
            let row = rows.iter().find(|r| r.arm == arm).unwrap();
            assert_eq!(row.producer_evidence, ProducerEvidence::Observed);
            assert!(!row.stale, "{arm} pends inside the young window");
            assert_eq!(row.cause.as_deref(), Some("daemon_young"));
            assert_ne!(row.cause.as_deref(), Some("scheduler_down"));
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_healthy_machine_reading_flips_nothing() {
        // Measured live at 2026-09-11T11:37Z: the positive control for a
        // false red.
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        write_rows(
            &journal,
            &[
                tick_envelope(
                    "2026-09-11T10:49:20Z",
                    "king_wake",
                    SCHED_LAUNCHD,
                    1,
                    json!(null),
                    900,
                ),
                tick_envelope(
                    "2026-09-11T10:52:52Z",
                    "watchdog",
                    SCHED_LAUNCHD,
                    1,
                    json!(null),
                    600,
                ),
                tick_envelope(
                    "2026-09-11T10:47:40Z",
                    "pr_watch_merge",
                    SCHED_LAUNCHD,
                    1,
                    json!(null),
                    600,
                ),
                tick_envelope(
                    "2026-09-11T10:49:51Z",
                    "notify_watch",
                    SCHED_LAUNCHD,
                    1,
                    json!(null),
                    300,
                ),
            ],
        );
        let now = parse_rfc3339_unix("2026-09-11T10:59:00Z").unwrap();

        let mut rows = read_arms(&[journal], now);
        explain(
            &mut rows,
            &DaemonFacts::Up {
                uptime_s: 40_000,
                drifted: false,
            },
        );
        for arm in ["king_wake", "watchdog", "pr_watch_merge", "notify_watch"] {
            let row = rows.iter().find(|r| r.arm == arm).unwrap();
            assert!(!row.stale, "{arm} must read ok, line: {}", row.line);
            assert_eq!(row.cause, None);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ts_parser_covers_both_shapes_and_known_points() {
        assert_eq!(parse_rfc3339_unix("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_unix("2026-09-04T12:00:00Z"),
            parse_rfc3339_unix("2026-09-04T12:00:00.123Z")
        );
        assert_eq!(
            parse_rfc3339_unix("2026-09-04T12:00:00Z"),
            Some(1_788_523_200)
        );
        assert_eq!(parse_rfc3339_unix("not-a-ts"), None);
    }
}
