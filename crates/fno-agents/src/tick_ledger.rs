//! One tick row per control-plane arm run, and the readout built from them.
//!
//! Every scheduled arm of the control plane (king wake, watchdog, pr-watch
//! merge dispatch, active backlog, auto-continue, the stop-hook shim) appends
//! one `control_plane_tick` row to the journal it already uses, saying what it
//! did or why it did nothing. The reader folds every journal into one row per
//! arm: last tick, last action, last skip reason, and a stale verdict when the
//! last tick is older than twice the arm's interval. The fold reads each
//! journal's committed store rows plus its live bytes
//! (`event_store::journal_text`): writers commit to the store only, so a fold
//! over the raw file stops at the store cutover. An arm whose journals
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
    /// The arm whose work this arm consumes. When the upstream arm is red,
    /// this arm's silence is the upstream's fault, and the row names it.
    pub upstream: Option<&'static str>,
    /// The config key that arms this loop. `None` means always armed.
    pub arm_key: Option<&'static str>,
    /// The verb that reads this loop's own detail, printed beside the row.
    pub reader: Option<&'static str>,
}

/// The resolved value of one arm key, as the row prints it. An unknown key
/// answers `unreadable`, never a silent blank - the same rule
/// `cause: Some("unexplained")` follows.
fn arm_key_value(cwd: &Path, key: &str) -> String {
    let answered = match key {
        "auto_heal.enabled" => crate::agents_config::auto_heal_enabled(cwd),
        "active_backlog.enabled" => crate::agents_config::active_backlog_enabled(cwd),
        _ => return "unreadable".to_string(),
    };
    if answered { "true" } else { "false" }.to_string()
}

/// Whether a row's arm key resolves false: the loop is off by config, its
/// silence is expected, and it never reads as attention-worthy.
pub fn row_is_unarmed(row: &ArmStatus) -> bool {
    row.arm_key.is_some() && row.arm_value.as_deref() == Some("false")
}

/// The upstream arm named in [`KNOWN_ARMS`], if any.
pub fn upstream_of(arm: &str) -> Option<&'static str> {
    KNOWN_ARMS
        .iter()
        .find(|s| s.arm == arm)
        .and_then(|s| s.upstream)
}

/// Every arm the readout shows, whether or not it has ever ticked.
pub const KNOWN_ARMS: &[ArmSpec] = &[
    ArmSpec {
        arm: "king_wake",
        default_interval_s: 900,
        scheduler: SCHED_LAUNCHD,
        upstream: None,
        arm_key: None,
        reader: None,
    },
    ArmSpec {
        arm: "watchdog",
        default_interval_s: 600,
        scheduler: SCHED_LAUNCHD,
        upstream: None,
        arm_key: None,
        reader: None,
    },
    ArmSpec {
        arm: "pr_watch_merge",
        default_interval_s: 600,
        scheduler: SCHED_LAUNCHD,
        upstream: None,
        arm_key: None,
        reader: Some("fno do pr watch status"),
    },
    ArmSpec {
        arm: "pr_watch_sweep",
        default_interval_s: 600,
        scheduler: SCHED_LAUNCHD,
        upstream: None,
        arm_key: None,
        reader: Some("fno do pr watch status"),
    },
    ArmSpec {
        arm: "active_backlog",
        default_interval_s: 300,
        scheduler: SCHED_DAEMON,
        upstream: None,
        arm_key: Some("active_backlog.enabled"),
        reader: Some("fno config active-backlog"),
    },
    ArmSpec {
        arm: "auto_continue",
        default_interval_s: 1800,
        scheduler: "session",
        upstream: Some("pr_watch_merge"),
        arm_key: None,
        reader: None,
    },
    ArmSpec {
        arm: "notify_watch",
        default_interval_s: 300,
        scheduler: SCHED_LAUNCHD,
        upstream: None,
        arm_key: None,
        reader: None,
    },
    ArmSpec {
        arm: "stop_hook",
        default_interval_s: 0,
        scheduler: "hook:target-stop-hook",
        upstream: None,
        arm_key: None,
        reader: None,
    },
    ArmSpec {
        arm: "reap",
        default_interval_s: 60,
        scheduler: SCHED_DAEMON,
        upstream: None,
        arm_key: None,
        reader: None,
    },
    ArmSpec {
        arm: "retire",
        default_interval_s: 300,
        scheduler: SCHED_DAEMON,
        upstream: None,
        arm_key: None,
        reader: None,
    },
    ArmSpec {
        arm: "machine_watch",
        default_interval_s: 300,
        scheduler: SCHED_DAEMON,
        upstream: None,
        arm_key: None,
        reader: None,
    },
    ArmSpec {
        arm: "arm_watch",
        default_interval_s: 300,
        scheduler: SCHED_DAEMON,
        upstream: None,
        arm_key: None,
        reader: Some("fno agents loops table"),
    },
    ArmSpec {
        arm: "provider_cap",
        default_interval_s: crate::provider_cap::PROVIDER_CAP_INTERVAL_S,
        scheduler: SCHED_DAEMON,
        upstream: None,
        arm_key: None,
        reader: None,
    },
    ArmSpec {
        arm: "merge_close",
        default_interval_s: crate::merge_close::MERGE_CLOSE_INTERVAL_S,
        scheduler: SCHED_DAEMON,
        upstream: None,
        arm_key: None,
        reader: None,
    },
    ArmSpec {
        arm: "crown_ledger",
        default_interval_s: crate::king_ledger::CROWN_LEDGER_INTERVAL_S,
        scheduler: SCHED_DAEMON,
        upstream: None,
        arm_key: None,
        reader: None,
    },
    ArmSpec {
        arm: "fleet_page",
        default_interval_s: crate::fleet_page::FLEET_PAGE_INTERVAL_S,
        scheduler: SCHED_DAEMON,
        upstream: None,
        arm_key: None,
        reader: None,
    },
    ArmSpec {
        arm: "attention",
        default_interval_s: crate::attention_arm::ATTENTION_INTERVAL_S,
        scheduler: SCHED_DAEMON,
        upstream: None,
        arm_key: None,
        reader: None,
    },
    ArmSpec {
        arm: "heal",
        default_interval_s: 600,
        scheduler: SCHED_LAUNCHD,
        upstream: None,
        arm_key: Some("auto_heal.enabled"),
        reader: Some("fno do pr watch status"),
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
    /// The one verb that repairs a red row, set by `arm_repair::annotate`.
    pub repair: Option<String>,
    /// Who runs the repair: `auto` (the arm_watch heal lane) or `operator`.
    pub heal: Option<String>,
    /// The red arm this row waits on, when its cause is `upstream_down`.
    pub upstream: Option<String>,
    /// The config key that arms this loop, from the spec. `None` for unknown
    /// arms and always-armed ones.
    pub arm_key: Option<String>,
    /// The key's resolved value (`true`/`false`/`unreadable`), read at fold
    /// time. `None` when the arm carries no key.
    pub arm_value: Option<String>,
    /// The verb that reads this loop's own detail, from the spec.
    pub reader: Option<String>,
    /// Every observed tick inside `notify.arm_starved_after_s` carried
    /// `acted=0` while the newest stayed fresh: the arm ran and produced
    /// nothing. Set by [`mark_starved`].
    pub starved: bool,
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

/// The row types the arms folds read: arm ticks and the healer's receipts.
const ARM_ROW_TYPES: &[&str] = &[EVENT_TYPE, "pr_heal_tick"];
/// The row types the pr-watch tick trace reads.
const TICK_TRACE_TYPES: &[&str] = &["pr_watch_tick_attempt", "pr_watch_tick_end"];

/// Every parsed row of `types` the journals hold: the store's committed rows,
/// then the live file's bytes. Writers commit to the store only, so a fold
/// over the raw file alone stops at the store cutover.
fn journal_rows(journals: &[PathBuf], types: &[&str]) -> Vec<Value> {
    journals
        .iter()
        .flat_map(|j| {
            crate::event_store::journal_text(j, types)
                .lines()
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Fold every journal into one row per known arm, reading each journal's
/// committed store rows plus its live bytes. Unknown arms seen in the
/// journals are appended after the known ones, so a new emitter deploys
/// before its reader does.
pub fn read_arms(journals: &[PathBuf], now_unix: u64) -> Vec<ArmStatus> {
    let mut newest: HashMap<String, NewestTick> = HashMap::new();
    let mut newest_ok: HashMap<String, NewestTick> = HashMap::new();
    for value in journal_rows(journals, ARM_ROW_TYPES) {
        fold_arm_row(value, &mut newest, &mut newest_ok);
    }

    // The static spec facts ride the fold; the runtime arm value is filled
    // by [`fill_arm_values`] at the readout layer, so this fold stays
    // machine-independent (and unit-testable without a config).
    let mut rows: Vec<ArmStatus> = KNOWN_ARMS
        .iter()
        .map(|spec| {
            let mut row = arm_status(
                spec.arm,
                Some(spec.scheduler),
                spec.default_interval_s,
                newest.get(spec.arm),
                newest_ok.get(spec.arm),
                now_unix,
            );
            row.arm_key = spec.arm_key.map(str::to_string);
            row.reader = spec.reader.map(str::to_string);
            row
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

fn fold_arm_row(
    value: Value,
    newest: &mut HashMap<String, NewestTick>,
    newest_ok: &mut HashMap<String, NewestTick>,
) {
    // The healer's receipt type folds into the `heal` arm here too, so
    // both journal folds agree on what a heal receipt looks like.
    let value = if value.get("type").and_then(Value::as_str) == Some("pr_heal_tick") {
        heal_tick_as_arm_row(value)
    } else {
        value
    };
    if value.get("type").and_then(Value::as_str) != Some(EVENT_TYPE) {
        return;
    }
    let Some(data) = value.get("data").and_then(Value::as_object) else {
        return;
    };
    let Some(arm) = data.get("arm").and_then(Value::as_str) else {
        return;
    };
    let Some(ts_unix) = value
        .get("ts")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_unix)
    else {
        return;
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

fn fresher_than(map: &HashMap<String, NewestTick>, arm: &str, ts_unix: u64) -> bool {
    match map.get(arm) {
        Some(seen) => ts_unix >= seen.ts_unix,
        None => true,
    }
}

/// One `pr_heal_tick` row folded into `control_plane_tick` shape for the
/// `heal` arm, so the readout sees a loop the arm rows alone never named:
/// the drive loop journals to the project it healed, while the arms fold
/// reads the agents home. acted=1 when the tick changed anything (rebased,
/// reran, escalated), 0 when it only looked; the counts ride `detail`.
fn heal_tick_as_arm_row(value: Value) -> Value {
    let mut data = value.get("data").cloned().unwrap_or(Value::Null);
    if let Some(obj) = data.as_object_mut() {
        let count = |obj: &serde_json::Map<String, Value>, k: &str| {
            obj.get(k).and_then(Value::as_u64).unwrap_or(0)
        };
        let acted =
            (count(obj, "rebased") + count(obj, "reran") + count(obj, "escalated") > 0) as u64;
        let detail = format!(
            "seen={} rebased={} reran={} escalated={} still_red={}",
            count(obj, "seen"),
            count(obj, "rebased"),
            count(obj, "reran"),
            count(obj, "escalated"),
            count(obj, "still_red")
        );
        obj.insert("arm".into(), json!("heal"));
        obj.insert("acted".into(), json!(acted));
        obj.insert("scheduler".into(), json!(SCHED_LAUNCHD));
        obj.insert("detail".into(), json!(detail));
    }
    json!({
        "type": EVENT_TYPE,
        "ts": value.get("ts").cloned().unwrap_or(Value::Null),
        "data": data,
    })
}

/// Mark rows whose arm is armed, ticking, and producing nothing: every
/// observed tick inside `threshold_s` carried `acted=0` with no skip reason
/// while the newest stayed fresh. A skip token that explains the idleness
/// (`calm`, `off_cadence`, a configured-off switch) is the arm stating its
/// own state, not starvation; and a stale or failing row keeps its louder
/// verdict. A heuristic with a ceiling - it reads a run of silent zeroes
/// over time, not the arm's input, so it tunes via the threshold knob,
/// never via an input probe.
pub fn mark_starved(journals: &[PathBuf], rows: &mut [ArmStatus], now_unix: u64, threshold_s: u64) {
    let mut history: HashMap<String, Vec<(u64, u64, bool)>> = HashMap::new();
    collect_tick_history(journal_rows(journals, ARM_ROW_TYPES), &mut history);
    for row in rows.iter_mut() {
        if row_is_unarmed(row)
            || row.producer_evidence == ProducerEvidence::Unobserved
            || row.stale
            || row.failing
        {
            continue;
        }
        let Some(ticks) = history.get(&row.arm) else {
            continue;
        };
        let window: Vec<&(u64, u64, bool)> = ticks
            .iter()
            .filter(|(ts, _, _)| *ts > now_unix.saturating_sub(threshold_s))
            .collect();
        if !window.is_empty()
            && window
                .iter()
                .all(|(_, acted, explained)| *acted == 0 && !explained)
        {
            row.starved = true;
        }
    }
}

/// Resolve every keyed row's `arm_value` against this machine's config. The
/// pure fold stays free of config reads; the readout layer owns them, so a
/// row's unarmed verdict reflects the config the reader actually runs.
pub fn fill_arm_values(rows: &mut [ArmStatus], cwd: &Path) {
    for row in rows.iter_mut() {
        if let Some(key) = row.arm_key.as_deref() {
            row.arm_value = Some(arm_key_value(cwd, key));
        }
    }
}

/// [`read_arms`] plus the arm values and the starved mark: the one read
/// every arms readout makes, so the table and the status arms never
/// disagree about the vocabulary. The threshold comes from
/// `notify.arm_starved_after_s`.
pub fn read_arms_starved(journals: &[PathBuf], now_unix: u64) -> Vec<ArmStatus> {
    let mut rows = read_arms(journals, now_unix);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    fill_arm_values(&mut rows, &cwd);
    let threshold = crate::agents_config::notify_arm_starved_after_s(&cwd);
    mark_starved(journals, &mut rows, now_unix, threshold);
    rows
}

/// One fold collecting every `(ts, acted, skip_explains)` triple an arm's
/// rows hold.
fn collect_tick_history(rows: Vec<Value>, history: &mut HashMap<String, Vec<(u64, u64, bool)>>) {
    for value in rows {
        let value = if value.get("type").and_then(Value::as_str) == Some("pr_heal_tick") {
            heal_tick_as_arm_row(value)
        } else {
            value
        };
        if value.get("type").and_then(Value::as_str) != Some(EVENT_TYPE) {
            continue;
        }
        let Some(data) = value.get("data") else {
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
        let skip_explains = data
            .get("skip_reason")
            .map(|v| !v.is_null())
            .unwrap_or(false);
        history.entry(arm.to_string()).or_default().push((
            ts_unix,
            data.get("acted").and_then(Value::as_u64).unwrap_or(0),
            skip_explains,
        ));
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
            repair: None,
            heal: None,
            upstream: None,
            arm_key: None,
            arm_value: None,
            reader: None,
            starved: false,
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
        repair: None,
        heal: None,
        upstream: None,
        arm_key: None,
        arm_value: None,
        reader: None,
        starved: false,
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
    if row_is_unarmed(row) {
        // An off loop is a configuration, not a fault: paging on it every
        // tick is the noise this table exists to remove.
        return false;
    }
    row.producer_evidence == ProducerEvidence::Unobserved || row.stale || row.failing
}

/// Skip reasons that mean the arm ran and its run failed - not that it chose
/// to skip. Sources: the pr-watch tick's outcome tokens (disabled, lock_held,
/// quota_skip pass through; timeout/error fail), the king-wake and notify
/// emitters' failure tokens, merge_close's `failures` (a partial reconcile
/// that left nodes unresolved), and auto_continue's `next-error` (a non-zero
/// or malformed `backlog next`) and `select-unmeasured` (a bounded read that
/// did not answer or a transient store refusal), `spawn-failed` (the dispatch it
/// fired exited non-zero), and active_backlog's `env_broken` (the resolver
/// shelled out and failed: no usable `fno`, non-zero exit, unreadable
/// receipt -): an arm that could not compute its input, or whose
/// action failed, has not skipped - it has failed. `select-unmeasured` is a
/// bounded selection that the arm_watch heal lane retries. `degraded` is
/// deliberately absent: it is emitted by an arm that ran and acted while one
/// read came back thin, and one transient gh read failure must not turn a
/// fresh row red.
const FAILURE_SKIPS: &[&str] = &[
    "timeout",
    "error",
    "failures",
    "next-error",
    "select-unmeasured",
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
    "unarmed",
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
    /// The phases the tick actually cut, from the end record's `cut` array.
    /// `None` means the record carried no `cut` key (a killed/errored tick,
    /// or a legacy record) - callers keep the pre-cut-list rule for those.
    pub end_cut: Option<Vec<String>>,
    /// Set when a stale launchd tier's registered plist lives outside the
    /// installer's LaunchAgents path - the 2026-09-08 shape where a pytest
    /// tempdir registration displaced the real pr-watcher.
    pub foreign_plist: Option<String>,
    /// The dispatch pause in force when the trace was read, `None` when
    /// clear. Set by the live read only: a folded journal trace cannot
    /// know whether a breaker is armed now. `DispatchPause` does not
    /// serialize, and the trace never needs to.
    #[serde(skip)]
    pub pause: Option<crate::loops_pause::DispatchPause>,
}

/// Fold the newest `pr_watch_tick_attempt` / `pr_watch_tick_end` records out
/// of each journal's committed store rows plus its live bytes. Absent
/// records leave defaults: the trace never invents a tick.
pub fn read_tick_trace(journals: &[PathBuf], now_unix: u64) -> TickTrace {
    let mut trace = TickTrace::default();
    for value in journal_rows(journals, TICK_TRACE_TYPES) {
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
            trace.end_cut = data.get("cut").and_then(Value::as_array).map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            });
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

/// The launchd labels the loops table lists, one row each with loaded yes/no
/// and the last exit `launchctl list` reports.
pub const LAUNCHD_LABELS: &[&str] = &[
    "sh.fno.pr-watcher",
    "sh.fno.groom",
    "sh.fno.autocontinue",
    "sh.fno.sync-backlog",
    "sh.fno.board-server",
    "com.user.autocorrect",
    "com.user.autocorrect-watcher",
];

/// One label's facts from the fold.
#[derive(Debug, Clone, Serialize)]
pub struct LaunchdLabelFacts {
    pub label: String,
    pub loaded: bool,
    pub last_exit: Option<i64>,
}

/// The launchd fold: every label the table lists with its loaded/last-exit
/// facts, plus the dead list any ``sh.fno.*`` or autocorrect label with a
/// nonzero last exit - the same shape doctor's deleted Python reader built,
/// widened to the labels its `sh.fno.` prefix filter missed. Not-applicable
/// off macOS or when launchctl cannot run: it fabricates no alarm.
#[derive(Debug, Clone, Serialize)]
pub struct LaunchdFold {
    pub applicable: bool,
    pub labels: Vec<LaunchdLabelFacts>,
    pub dead: Vec<LaunchdLabelFacts>,
}

/// Run the fold live. `None` off macOS or when `launchctl list` fails or
/// outlives its 5s bound - a wedged launchctl must not wedge the readout.
pub fn launchd_fold_live() -> Option<LaunchdFold> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let cmd = vec!["launchctl".to_string(), "list".to_string()];
    let stdout = crate::king_board::budget::run_with_timeout(
        &cmd,
        Path::new("."),
        std::time::Duration::from_secs(5),
    )
    .ok()?;
    Some(parse_launchctl_list(&String::from_utf8_lossy(&stdout)))
}

/// Pure fold so tests run without launchctl. Loaded = the label appeared in
/// `launchctl list`; last exit is column 2; a `-` is no measured exit. The
/// header row is skipped by name, not by position, so a header-less listing
/// never loses its first label.
pub(crate) fn parse_launchctl_list(text: &str) -> LaunchdFold {
    let mut labels: Vec<LaunchdLabelFacts> = LAUNCHD_LABELS
        .iter()
        .map(|l| LaunchdLabelFacts {
            label: (*l).to_string(),
            loaded: false,
            last_exit: None,
        })
        .collect();
    for line in text.lines() {
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 3 || cols[0].trim() == "PID" {
            continue;
        }
        let label = cols[2].trim();
        if let Some(slot) = labels.iter_mut().find(|f| f.label == label) {
            slot.loaded = true;
            slot.last_exit = cols[1].trim().parse::<i64>().ok();
        }
    }
    let dead: Vec<LaunchdLabelFacts> = text
        .lines()
        .filter_map(|line| {
            let cols: Vec<&str> = line.split('\t').collect();
            if cols.len() < 3 || cols[0].trim() == "PID" {
                return None;
            }
            let label = cols[2].trim();
            if !label.starts_with("sh.fno.")
                && label != "com.user.autocorrect"
                && label != "com.user.autocorrect-watcher"
            {
                return None;
            }
            let exit: i64 = cols[1].trim().parse().ok()?;
            (exit != 0).then(|| LaunchdLabelFacts {
                label: label.to_string(),
                loaded: true,
                last_exit: Some(exit),
            })
        })
        .collect();
    LaunchdFold {
        applicable: true,
        labels,
        dead,
    }
}

/// [`read_tick_trace`] plus one live launchd probe: when any launchd-scheduled
/// arm is stale, read the registered plist path so the cause can name a
/// foreign registration instead of a bare tick_overdue. macOS only; bounded to
/// 2s; every failure mode (nonzero exit, kill, absent HOME) leaves the trace
/// untouched. A healthy tier runs no exec at all.
pub fn read_tick_trace_live(journals: &[PathBuf], rows: &[ArmStatus], now_unix: u64) -> TickTrace {
    let mut trace = read_tick_trace(journals, now_unix);
    // The one live pause read, taken before the tier check so a healthy
    // tier carries the fact too (the king summary reads it there). A
    // journal-folded trace stays pause-free.
    trace.pause = Some(crate::loops_pause::dispatch_pause())
        .filter(crate::loops_pause::DispatchPause::is_paused);
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
            let pm = &mut rows[i];
            pm.failing = true;
            // The synthesized failure is not an absence: the row itself was
            // the newest healthy run, so it anchors the duration too.
            pm.failing_for_s = pm.age_s;
            pm.cause = Some("tick_timeout".to_string());
            pm_tick_hint = Some(if t.end_cut.is_some() {
                format!("ended {outcome} with phase merge cut; run fno do pr watch status")
            } else {
                let phase = t.end_phase.as_deref().unwrap_or("unknown");
                format!(
                    "the tick containing this phase ended {outcome} in phase {phase}; \
                     run fno do pr watch status"
                )
            });
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
            let (mut cause, mut hint) =
                if pm_stale && row.scheduler.as_deref() == Some(SCHED_LAUNCHD) {
                    tick_overdue_cause(pm_last_ts.as_deref(), trace)
                } else {
                    let cause =
                        stale_cause(row, daemon, pm_fresh_failure, trace).unwrap_or_else(|| {
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
            // An armed breaker or a hand-pause explains a whole silent
            // tier by itself: the cause tokens that blame tick recency or
            // a dead scheduler are wrong under it, and the pause outranks
            // them. Real faults (foreign plist, dead daemon, timeouts)
            // keep their cause, and an unreadable breaker stays a fault.
            let pause = trace.and_then(|t| t.pause.as_ref());
            if let Some(p) = pause {
                if matches!(
                    cause.as_str(),
                    "tick_overdue" | "scheduler_down" | "unexplained"
                ) {
                    cause = p.skip_reason().to_string();
                    hint = match p {
                        crate::loops_pause::DispatchPause::FleetIncident { .. }
                        | crate::loops_pause::DispatchPause::Manual { .. } => {
                            format!(
                                "{}; held on purpose; wait for the breaker to clear",
                                p.detail()
                            )
                        }
                        crate::loops_pause::DispatchPause::FleetIncidentUnavailable { .. } => {
                            p.detail()
                        }
                        crate::loops_pause::DispatchPause::Clear => {
                            unreachable!("a Clear pause never enters the trace")
                        }
                    };
                    if !matches!(
                        p,
                        crate::loops_pause::DispatchPause::FleetIncidentUnavailable { .. }
                    ) {
                        row.stale = false;
                    }
                }
            }
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
    if end_ts <= row_ts {
        return false;
    }
    match trace.end_cut.as_deref() {
        Some(cut) => cut.iter().any(|p| p == "merge"),
        None => true,
    }
}

/// The first cause that holds for a stale row, in table order; `None` leaves
/// the row to `unexplained`. A stale launchd tier with a stale
/// pr_watch_merge is handled by the caller: it reads the tick trace and
/// answers `tick_overdue` with evidence, not this table.
fn stale_cause(
    row: &ArmStatus,
    daemon: &DaemonFacts,
    pm_fresh_failure: bool,
    trace: Option<&TickTrace>,
) -> Option<String> {
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
    if sched == Some(SCHED_LAUNCHD) && row.arm != "pr_watch_merge" {
        return match trace.and_then(|t| t.end_cut.as_deref()) {
            Some(cut) => cut
                .iter()
                .any(|p| p == &row.arm)
                .then(|| "tick_timeout".to_string()),
            None => pm_fresh_failure.then(|| "tick_timeout".to_string()),
        };
    }
    None
}

/// The human hint appended after each cause token. The text lives in the one
/// cause table, `arm_repair`; only the young-daemon hint carries a live number.
fn cause_hint(cause: &str, daemon: &DaemonFacts) -> String {
    match (cause, daemon) {
        ("daemon_young", DaemonFacts::Up { uptime_s, .. }) => {
            format!("daemon up {uptime_s}s, first window not elapsed")
        }
        _ => crate::arm_repair::hint(cause).to_string(),
    }
}

/// The per-row readout format, owned here so every consumer prints the same
/// line. Verdict: unarmed when the arm key resolves false, before every
/// other verdict: the loop is off by config, so absence of receipts is
/// expected and absence of receipts is not staleness, failure, or ok.
/// UNOBSERVED follows it. STALE when stale, FAIL when failing, pending when
/// the cause is daemon_young, else ok. UPSTREAM outranks STALE and FAIL:
/// the arm is waiting on a red arm, not broken. starved claims only
/// otherwise-ok rows: an arm running on a fresh receipt that produced
/// nothing inside the threshold window, and whose ticks never named a skip
/// reason. The `cause=...` suffix is appended by `explain`.
pub fn render_row(row: &ArmStatus) -> String {
    let verdict = if row_is_unarmed(row) {
        "unarmed"
    } else if row.producer_evidence == ProducerEvidence::Unobserved {
        "UNOBSERVED"
    } else if row.cause.as_deref() == Some("upstream_down") {
        "UPSTREAM"
    } else if row.stale {
        "STALE"
    } else if row.failing {
        "FAIL"
    } else if row.cause.as_deref() == Some("daemon_young") {
        "pending"
    } else if matches!(
        row.cause.as_deref(),
        Some("fleet_stop") | Some("loops_paused")
    ) {
        "PAUSED"
    } else if row.starved {
        "starved"
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
    let key = match (&row.arm_key, &row.arm_value) {
        (Some(k), Some(v)) => format!(" key={k}={v}"),
        _ => String::new(),
    };
    format!(
        "{arm:<16} {verdict:<5} {age:>10}{acted}{skip}{scheduler}{key}{detail}{failing_for}",
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
        // A counter joins pid+nanos: same-process tests can read the same
        // coarse nanos on macOS, and a collided name made one test's
        // remove_dir_all delete another test's journal mid-read.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "fno-tick-ledger-{}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
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
        assert_eq!(KNOWN_ARMS.len(), 18);
        let attention = KNOWN_ARMS
            .iter()
            .find(|s| s.arm == "attention")
            .expect("attention row");
        assert_eq!(
            attention.default_interval_s,
            crate::attention_arm::ATTENTION_INTERVAL_S
        );
        assert_eq!(attention.scheduler, SCHED_DAEMON);
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
        let fleet = KNOWN_ARMS
            .iter()
            .find(|s| s.arm == "fleet_page")
            .expect("fleet_page row");
        assert_eq!(
            fleet.default_interval_s,
            crate::fleet_page::FLEET_PAGE_INTERVAL_S
        );
        assert_eq!(fleet.scheduler, SCHED_DAEMON);
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
    fn unarmed_row_reads_unarmed_names_its_key_and_needs_no_attention() {
        let mut row = arm_status("heal", Some(SCHED_LAUNCHD), 600, None, None, 0);
        row.arm_key = Some("auto_heal.enabled".into());
        row.arm_value = Some("false".into());
        let line = render_row(&row);
        assert!(line.contains(" unarmed "), "{line}");
        assert!(line.contains("key=auto_heal.enabled=false"), "{line}");
        assert!(!needs_attention(&row));
    }

    #[test]
    fn starved_when_every_in_window_tick_is_a_no_op() {
        let dir = temp_dir();
        let path = dir.join("events.jsonl");
        write_rows(
            &path,
            &[
                tick_envelope(
                    "2026-09-14T11:00:00Z",
                    "heal",
                    SCHED_LAUNCHD,
                    1,
                    json!(null),
                    600,
                ),
                tick_envelope(
                    "2026-09-20T12:00:00Z",
                    "heal",
                    SCHED_LAUNCHD,
                    0,
                    json!(null),
                    600,
                ),
                tick_envelope(
                    "2026-09-21T11:59:00Z",
                    "heal",
                    SCHED_LAUNCHD,
                    0,
                    json!(null),
                    600,
                ),
            ],
        );
        let now = parse_rfc3339_unix("2026-09-21T12:00:00Z").unwrap();
        let journals = vec![path.clone()];
        let mut rows = read_arms(&journals, now);
        mark_starved(&journals, &mut rows, now, 604_800);
        let heal = rows.iter().find(|r| r.arm == "heal").expect("heal row");
        assert!(heal.starved, "{heal:?}");
        assert!(
            render_row(heal).contains(" starved "),
            "{}",
            render_row(heal)
        );
        assert!(!needs_attention(heal));
    }

    #[test]
    fn one_acted_tick_in_the_window_reads_ok_not_starved() {
        let dir = temp_dir();
        let path = dir.join("events.jsonl");
        write_rows(
            &path,
            &[
                tick_envelope(
                    "2026-09-14T11:00:00Z",
                    "heal",
                    SCHED_LAUNCHD,
                    1,
                    json!(null),
                    600,
                ),
                tick_envelope(
                    "2026-09-20T12:00:00Z",
                    "heal",
                    SCHED_LAUNCHD,
                    1,
                    json!(null),
                    600,
                ),
                tick_envelope(
                    "2026-09-21T11:59:00Z",
                    "heal",
                    SCHED_LAUNCHD,
                    0,
                    json!(null),
                    600,
                ),
            ],
        );
        let now = parse_rfc3339_unix("2026-09-21T12:00:00Z").unwrap();
        let journals = vec![path.clone()];
        let mut rows = read_arms(&journals, now);
        mark_starved(&journals, &mut rows, now, 604_800);
        let heal = rows.iter().find(|r| r.arm == "heal").expect("heal row");
        assert!(!heal.starved);
    }

    #[test]
    fn an_explained_skip_or_a_failing_row_never_reads_starved() {
        let dir = temp_dir();
        let path = dir.join("events.jsonl");
        // Every in-window tick idles, but each names its skip reason: the arm
        // states its own idleness, which is not starvation.
        write_rows(
            &path,
            &[
                tick_envelope(
                    "2026-09-20T12:00:00Z",
                    "heal",
                    SCHED_LAUNCHD,
                    0,
                    json!("calm"),
                    600,
                ),
                tick_envelope(
                    "2026-09-21T11:59:00Z",
                    "heal",
                    SCHED_LAUNCHD,
                    0,
                    json!("calm"),
                    600,
                ),
            ],
        );
        let now = parse_rfc3339_unix("2026-09-21T12:00:00Z").unwrap();
        let journals = vec![path.clone()];
        let mut rows = read_arms(&journals, now);
        mark_starved(&journals, &mut rows, now, 604_800);
        let heal = rows.iter().find(|r| r.arm == "heal").expect("heal row");
        assert!(!heal.starved);
    }

    #[test]
    fn a_failing_arm_keeps_fail_and_never_reads_starved() {
        let dir = temp_dir();
        let path = dir.join("events.jsonl");
        write_rows(
            &path,
            &[
                tick_envelope(
                    "2026-09-20T12:00:00Z",
                    "heal",
                    SCHED_LAUNCHD,
                    0,
                    json!(null),
                    600,
                ),
                tick_envelope(
                    "2026-09-21T11:59:00Z",
                    "heal",
                    SCHED_LAUNCHD,
                    0,
                    json!("timeout"),
                    600,
                ),
            ],
        );
        let now = parse_rfc3339_unix("2026-09-21T12:00:00Z").unwrap();
        let journals = vec![path.clone()];
        let mut rows = read_arms(&journals, now);
        mark_starved(&journals, &mut rows, now, 604_800);
        let heal = rows.iter().find(|r| r.arm == "heal").expect("heal row");
        assert!(heal.failing);
        assert!(!heal.starved);
        assert_eq!(render_row(heal).split_whitespace().nth(1), Some("FAIL"));
    }

    #[test]
    fn heal_row_folds_pr_heal_tick_receipts() {
        let dir = temp_dir();
        let path = dir.join("events.jsonl");
        write_rows(
            &path,
            &[json!({
                "ts": "2026-09-21T11:30:00Z",
                "type": "pr_heal_tick",
                "source": "pr-heal",
                "data": {
                    "root": "/tmp/proj",
                    "seen": 5, "skip_deadline": 4, "still_red": 1,
                    "unknown": 2, "dry_run": false,
                    "rebased": 0, "escalated": 3, "reran": 0,
                    "duration_s": 70.5,
                }
            })],
        );
        let now = parse_rfc3339_unix("2026-09-21T12:00:00Z").unwrap();
        let rows = read_arms(&vec![path.clone()], now);
        let heal = rows.iter().find(|r| r.arm == "heal").expect("heal row");
        assert_eq!(heal.producer_evidence, ProducerEvidence::Observed);
        assert_eq!(heal.acted, Some(1));
        assert!(heal.detail.as_deref().unwrap_or("").contains("escalated=3"));
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

        let text = crate::events::committed_journal_text(&project);
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

    fn commit_row(journal: &Path, row: &Value) {
        crate::event_store::append_envelope(journal, &serde_json::to_string(row).unwrap(), None)
            .unwrap();
    }

    #[test]
    fn a_row_committed_only_to_the_store_reads_fresh() {
        let dir = temp_dir();
        let journal = dir.join("events.jsonl");
        std::fs::create_dir_all(&dir).unwrap();
        let row = tick_envelope(
            "2026-09-22T08:27:07Z",
            "king_wake",
            SCHED_DAEMON,
            1,
            json!(null),
            900,
        );
        commit_row(&journal, &row);
        let now = parse_rfc3339_unix("2026-09-22T08:27:17Z").unwrap();
        let rows = read_arms(&[journal.clone()], now);
        let king = rows
            .iter()
            .find(|r| r.arm == "king_wake")
            .expect("king_wake row");
        assert_eq!(king.producer_evidence, ProducerEvidence::Observed);
        assert_eq!(king.age_s, Some(10));
        assert!(!king.stale);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unreadable_store_falls_back_to_the_live_rows() {
        let dir = temp_dir();
        let journal = dir.join("events.jsonl");
        write_rows(
            &journal,
            &[tick_envelope(
                "2026-09-22T05:00:00Z",
                "king_wake",
                SCHED_DAEMON,
                1,
                json!(null),
                900,
            )],
        );
        std::fs::write(dir.join("events.db"), b"not a sqlite database").unwrap();
        let now = parse_rfc3339_unix("2026-09-22T05:00:10Z").unwrap();
        let rows = read_arms(&[journal], now);
        let king = rows
            .iter()
            .find(|r| r.arm == "king_wake")
            .expect("king_wake row");
        assert_eq!(king.acted, Some(1));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_newer_store_row_outranks_a_frozen_live_row() {
        let dir = temp_dir();
        let journal = dir.join("events.jsonl");
        let frozen = tick_envelope(
            "2026-09-22T04:51:51Z",
            "king_wake",
            SCHED_DAEMON,
            1,
            json!(null),
            900,
        );
        let fresh = tick_envelope(
            "2026-09-22T08:27:07Z",
            "king_wake",
            SCHED_DAEMON,
            0,
            json!("no_trigger"),
            900,
        );
        write_rows(&journal, &[frozen.clone()]);
        commit_row(&journal, &frozen);
        commit_row(&journal, &fresh);
        let now = parse_rfc3339_unix("2026-09-22T08:27:17Z").unwrap();
        let rows = read_arms(&[journal], now);
        let king = rows
            .iter()
            .find(|r| r.arm == "king_wake")
            .expect("king_wake row");
        assert_eq!(king.skip_reason.as_deref(), Some("no_trigger"));
        assert_eq!(king.age_s, Some(10));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn store_committed_no_op_ticks_mark_the_arm_starved() {
        let dir = temp_dir();
        let journal = dir.join("events.jsonl");
        std::fs::create_dir_all(&dir).unwrap();
        for ts in [
            "2026-09-22T08:20:00Z",
            "2026-09-22T08:25:00Z",
            "2026-09-22T08:30:00Z",
        ] {
            let row = tick_envelope(ts, "watchdog", SCHED_DAEMON, 0, json!(null), 600);
            commit_row(&journal, &row);
        }
        let now = parse_rfc3339_unix("2026-09-22T08:30:10Z").unwrap();
        let mut rows = read_arms(&[journal.clone()], now);
        mark_starved(&[journal], &mut rows, now, 3_600);
        let wd = rows
            .iter()
            .find(|r| r.arm == "watchdog")
            .expect("watchdog row");
        assert!(wd.starved);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_tick_trace_reads_a_store_committed_end() {
        let dir = temp_dir();
        let journal = dir.join("events.jsonl");
        std::fs::create_dir_all(&dir).unwrap();
        let row = json!({
            "ts": "2026-09-22T08:27:07Z",
            "type": "pr_watch_tick_end",
            "source": "pr-watch",
            "data": {"phase": "merge", "outcome": "ok", "cut": ["merge"]},
        });
        commit_row(&journal, &row);
        let now = parse_rfc3339_unix("2026-09-22T08:27:17Z").unwrap();
        let trace = read_tick_trace(&[journal], now);
        assert_eq!(trace.end_phase.as_deref(), Some("merge"));
        assert_eq!(trace.end_outcome.as_deref(), Some("ok"));
        assert_eq!(trace.end_cut, Some(vec!["merge".to_string()]));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn newest_row_per_arm_wins_across_journals() {
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
            repair: None,
            heal: None,
            upstream: None,
            arm_key: None,
            arm_value: None,
            reader: None,
            starved: false,
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
            repair: None,
            heal: None,
            upstream: None,
            arm_key: None,
            arm_value: None,
            reader: None,
            starved: false,
        };
        let cause = stale_cause(&row, &DaemonFacts::Down, false, None).unwrap();
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
    fn a_partial_reconcile_failures_skip_fails_the_merge_close_arm() {
        // failures is merge_close's partial-sweep token: the sweep ran and
        // left nodes unresolved. It must read FAIL like error, never ok -
        // a healthy read with nodes left open is the bug this closes.
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        // ok tick 3600s ago, failures tick 120s ago (interval 900: fresh).
        write_rows(
            &journal,
            &[
                tick_envelope(
                    "2026-09-04T11:00:00Z",
                    "merge_close",
                    SCHED_DAEMON,
                    1,
                    json!(null),
                    900,
                ),
                tick_envelope(
                    "2026-09-04T11:58:00Z",
                    "merge_close",
                    SCHED_DAEMON,
                    0,
                    json!("failures"),
                    900,
                ),
            ],
        );
        let now = parse_rfc3339_unix("2026-09-04T12:00:00Z").unwrap();

        let rows = read_arms(&[journal], now);
        let mc = rows.iter().find(|r| r.arm == "merge_close").unwrap();
        assert!(mc.failing, "failures must set failing");
        assert_eq!(mc.failing_for_s, Some(3600));
        let line = render_row(mc);
        assert!(line.contains("FAIL"), "line: {line}");
        assert!(line.contains("skip=failures"), "line: {line}");
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
    fn a_fresh_select_unmeasured_fails_the_auto_continue_arm() {
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        write_rows(
            &journal,
            &[tick_envelope(
                "2026-09-04T11:58:20Z",
                "auto_continue",
                "session",
                0,
                json!("select-unmeasured"),
                1800,
            )],
        );
        let now = parse_rfc3339_unix("2026-09-04T12:00:00Z").unwrap();

        let mut rows = read_arms(&[journal], now);
        explain(&mut rows, &DaemonFacts::Unknown);
        let ac = rows.iter().find(|r| r.arm == "auto_continue").unwrap();
        assert!(ac.failing, "an unmeasured selection is a failed run");
        assert!(
            ac.line.contains("skip=select-unmeasured"),
            "line: {}",
            ac.line
        );
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
            kw.line.contains("the pr-watch tick cut this arm's phase"),
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
        crate::paths::pin_test_claims_root(&dir);
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
    fn a_cut_list_without_merge_leaves_the_merge_row_ok() {
        // AC1-HP: the tick timed out, but the cut list names other phases.
        // The merge phase ran, so the row keeps its own ok.
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        write_rows(
            &journal,
            &[tick_envelope(
                "2026-09-16T11:58:20Z",
                "pr_watch_merge",
                SCHED_LAUNCHD,
                3,
                json!(null),
                600,
            )],
        );
        let now = parse_rfc3339_unix("2026-09-16T12:00:00Z").unwrap();
        let trace = TickTrace {
            end_ts_unix: Some(parse_rfc3339_unix("2026-09-16T11:59:30Z").unwrap()),
            end_outcome: Some("timeout".to_string()),
            end_cut: Some(vec!["sweep".to_string(), "king_wake".to_string()]),
            ..TickTrace::default()
        };

        let mut rows = read_arms(&[journal], now);
        explain_with_trace(&mut rows, &DaemonFacts::Unknown, &trace);
        let pm = rows.iter().find(|r| r.arm == "pr_watch_merge").unwrap();
        assert!(!pm.failing, "line: {}", pm.line);
        assert_eq!(pm.cause, None);
        assert!(!pm.line.contains("FAIL"), "line: {}", pm.line);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_cut_list_naming_merge_fails_the_merge_row() {
        // AC2-HP: the cut list names the merge phase itself, so the tick
        // timeout is a real merge fault.
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        write_rows(
            &journal,
            &[tick_envelope(
                "2026-09-16T11:58:20Z",
                "pr_watch_merge",
                SCHED_LAUNCHD,
                3,
                json!(null),
                600,
            )],
        );
        let now = parse_rfc3339_unix("2026-09-16T12:00:00Z").unwrap();
        let trace = TickTrace {
            end_ts_unix: Some(parse_rfc3339_unix("2026-09-16T11:59:30Z").unwrap()),
            end_outcome: Some("timeout".to_string()),
            end_cut: Some(vec!["merge".to_string(), "recovery".to_string()]),
            ..TickTrace::default()
        };

        let mut rows = read_arms(&[journal], now);
        explain_with_trace(&mut rows, &DaemonFacts::Unknown, &trace);
        let pm = rows.iter().find(|r| r.arm == "pr_watch_merge").unwrap();
        assert!(pm.failing, "line: {}", pm.line);
        assert_eq!(pm.cause.as_deref(), Some("tick_timeout"));
        assert!(pm.line.contains("FAIL"), "line: {}", pm.line);
        assert!(pm.line.contains("merge"), "line: {}", pm.line);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_stale_arm_named_in_the_cut_list_blames_the_tick() {
        // AC4-HP: king_wake is stale and the cut list names its own phase.
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        write_rows(
            &journal,
            &[
                tick_envelope(
                    "2026-09-11T09:52:20Z",
                    "king_wake",
                    SCHED_LAUNCHD,
                    0,
                    json!(null),
                    900,
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
        let trace = TickTrace {
            end_ts_unix: Some(parse_rfc3339_unix("2026-09-11T10:58:00Z").unwrap()),
            end_outcome: Some("timeout".to_string()),
            end_cut: Some(vec!["king_wake".to_string(), "stranded".to_string()]),
            ..TickTrace::default()
        };

        let mut rows = read_arms(&[journal], now);
        explain_with_trace(&mut rows, &DaemonFacts::Unknown, &trace);
        let kw = rows.iter().find(|r| r.arm == "king_wake").unwrap();
        assert!(kw.stale, "line: {}", kw.line);
        assert_eq!(kw.cause.as_deref(), Some("tick_timeout"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_stale_arm_left_out_of_the_cut_list_does_not_blame_the_tick() {
        // AC5-ERR: notify_watch is stale but the cut list names a different
        // phase, so the tick is not the reason this arm went quiet.
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
                    "pr_watch_merge",
                    SCHED_LAUNCHD,
                    0,
                    json!(null),
                    600,
                ),
            ],
        );
        let now = parse_rfc3339_unix("2026-09-11T10:59:00Z").unwrap();
        let trace = TickTrace {
            end_ts_unix: Some(parse_rfc3339_unix("2026-09-11T10:58:00Z").unwrap()),
            end_outcome: Some("timeout".to_string()),
            end_cut: Some(vec!["sweep".to_string()]),
            ..TickTrace::default()
        };

        let mut rows = read_arms(&[journal], now);
        explain_with_trace(&mut rows, &DaemonFacts::Unknown, &trace);
        let nw = rows.iter().find(|r| r.arm == "notify_watch").unwrap();
        assert!(nw.stale, "line: {}", nw.line);
        assert_ne!(
            nw.cause.as_deref(),
            Some("tick_timeout"),
            "line: {}",
            nw.line
        );
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

    /// The AC1 shape, shared by the pause tests: four launchd arms past
    /// 2x interval, a tick trace whose last end is phase entry, outcome
    /// paused, and a pause fact set by hand. Tests never read or flip the
    /// real breaker.
    fn paused_tier_rows(now: &str) -> (PathBuf, Vec<ArmStatus>, TickTrace) {
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        write_rows(
            &journal,
            &[
                tick_envelope(
                    "2026-09-17T22:30:00Z",
                    "king_wake",
                    SCHED_LAUNCHD,
                    0,
                    json!(null),
                    900,
                ),
                tick_envelope(
                    "2026-09-17T22:30:00Z",
                    "watchdog",
                    SCHED_LAUNCHD,
                    0,
                    json!(null),
                    900,
                ),
                tick_envelope(
                    "2026-09-17T22:30:00Z",
                    "pr_watch_merge",
                    SCHED_LAUNCHD,
                    0,
                    json!(null),
                    600,
                ),
                tick_envelope(
                    "2026-09-17T22:30:00Z",
                    "notify_watch",
                    SCHED_LAUNCHD,
                    0,
                    json!(null),
                    300,
                ),
            ],
        );
        let now_unix = parse_rfc3339_unix(now).unwrap();
        let rows = read_arms(&[journal.clone()], now_unix);
        let trace = TickTrace {
            end_ts_unix: Some(parse_rfc3339_unix("2026-09-17T23:34:07Z").unwrap()),
            end_phase: Some("entry".to_string()),
            end_outcome: Some("paused".to_string()),
            pause: Some(crate::loops_pause::DispatchPause::FleetIncident {
                generation: 5,
                reason: "two cargo runs".to_string(),
            }),
            ..TickTrace::default()
        };
        (journal, rows, trace)
    }

    #[test]
    fn an_armed_breaker_reads_paused_and_names_its_generation() {
        let (dir, mut rows, trace) = paused_tier_rows("2026-09-17T23:40:00Z");
        explain_with_trace(&mut rows, &DaemonFacts::Unknown, &trace);
        for arm in ["king_wake", "watchdog", "pr_watch_merge", "notify_watch"] {
            let row = rows.iter().find(|r| r.arm == arm).unwrap();
            assert!(!row.stale, "{arm} is held on purpose, line: {}", row.line);
            assert_eq!(
                row.cause.as_deref(),
                Some("fleet_stop"),
                "line: {}",
                row.line
            );
            assert!(row.line.contains("PAUSED"), "line: {}", row.line);
            assert!(row.line.contains("generation 5"), "line: {}", row.line);
            assert!(row.line.contains("two cargo runs"), "line: {}", row.line);
            assert!(
                !row.line.contains("tick_overdue"),
                "{arm} must not read tick_overdue, line: {}",
                row.line
            );
            assert!(
                !row.line.contains("pr watch refresh"),
                "{arm} must not prescribe a refresh, line: {}",
                row.line
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn without_a_pause_fact_the_tier_keeps_tick_overdue() {
        let (dir, mut rows, trace) = paused_tier_rows("2026-09-17T23:40:00Z");
        let trace = TickTrace {
            pause: None,
            ..trace
        };
        explain_with_trace(&mut rows, &DaemonFacts::Unknown, &trace);
        let kw = rows.iter().find(|r| r.arm == "king_wake").unwrap();
        assert!(kw.stale, "line: {}", kw.line);
        assert_eq!(
            kw.cause.as_deref(),
            Some("tick_overdue"),
            "line: {}",
            kw.line
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_real_fault_outranks_the_pause() {
        let (dir, mut rows, trace) = paused_tier_rows("2026-09-17T23:40:00Z");
        let trace = TickTrace {
            foreign_plist: Some("/tmp/pytest-xyz/sh.fno.pr-watcher.plist".to_string()),
            ..trace
        };
        explain_with_trace(&mut rows, &DaemonFacts::Unknown, &trace);
        let kw = rows.iter().find(|r| r.arm == "king_wake").unwrap();
        assert_eq!(
            kw.cause.as_deref(),
            Some("launchd_foreign_plist"),
            "line: {}",
            kw.line
        );
        assert!(kw.stale, "line: {}", kw.line);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unreadable_breaker_stays_a_fault() {
        let (dir, mut rows, trace) = paused_tier_rows("2026-09-17T23:40:00Z");
        let trace = TickTrace {
            pause: Some(
                crate::loops_pause::DispatchPause::FleetIncidentUnavailable {
                    detail: "no fleet-stop record".to_string(),
                },
            ),
            ..trace
        };
        explain_with_trace(&mut rows, &DaemonFacts::Unknown, &trace);
        let kw = rows.iter().find(|r| r.arm == "king_wake").unwrap();
        assert!(kw.stale, "line: {}", kw.line);
        assert_eq!(
            kw.cause.as_deref(),
            Some("fleet_stop_unavailable"),
            "line: {}",
            kw.line
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
