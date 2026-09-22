//! The fleet load engine behind `fno-agents intel --fleet`: what load the
//! fleet put on this machine, hour by hour, and what session cap follows.
//!
//! Sources, all of them history fno already owns: structured machine_sample
//! rows written by the Rust machine_watch arm, machine_watch tick rows (a hot
//! row says `crosses band` and load_15m can read `unavailable`), flat
//! `spawn_gate_refused` rows
//! (`kind`, top-level `ts`), `reign_checkin` rows whose `data.live_workers`
//! is numeric (34 of 138 stored rows carry it), and the incremental transcript
//! fold in [crate::transcript_activity].
//!
//! Every path comes from fno config or a harness store resolver: no user,
//! uid or repo name appears here.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

use crate::paths::AgentsHome;
use crate::transcript_activity::{
    hour_string, Activity, FoldReceipt, Roots, Slowdown, TICK_READ_BUDGET,
};

/// The verb's default budget: bounded, so a plain `--fleet` never scans the
/// whole corpus; `--backfill` lifts it.
pub(crate) const DEFAULT_WINDOW_DAYS: u64 = 30;

/// One machine_watch reading parsed from the tick detail sentence.
#[derive(Debug, Clone, Serialize)]
pub struct Reading {
    pub ts: String,
    pub ts_ms: i64,
    pub busy_pct: f64,
    pub band_pct: f64,
    pub cores: f64,
    pub verdict: String,
    pub load_15m: Option<f64>,
    pub runnable: Option<f64>,
    pub processes: Option<f64>,
}

fn reading_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(concat!(
            r"machine (\d+(?:\.\d+)?)% (?:of|crosses) band (\d+(?:\.\d+)?)% ",
            r"\((\d+(?:\.\d+)?) of (\d+(?:\.\d+)?) cores\) -> (\w+); ",
            r"load_15m (\d+(?:\.\d+)?|unavailable), (\d+|None) runnable of (\d+|None) processes",
        ))
        .expect("valid pattern")
    })
}

/// Parse one tick detail sentence. Searched, never anchored: a hot row
/// starts `1/2 hot: ` or `notified: `.
pub(crate) fn parse_reading(detail: &str, ts_ms: i64, ts: &str) -> Option<Reading> {
    let caps = reading_re().captures(detail)?;
    let num = |i: usize| caps.get(i).and_then(|m| m.as_str().parse::<f64>().ok());
    Some(Reading {
        ts: ts.to_string(),
        ts_ms,
        busy_pct: num(1)?,
        band_pct: num(2)?,
        cores: num(3)?,
        verdict: caps.get(5)?.as_str().to_string(),
        load_15m: num(6),
        runnable: num(7),
        processes: num(8),
    })
}

fn median(values: &mut Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    let mid = values.len() / 2;
    Some(if values.len() % 2 == 1 {
        values[mid]
    } else {
        (values[mid - 1] + values[mid]) / 2.0
    })
}

/// 25th percentile, nearest rank: `sorted[ceil(0.25*n) - 1]`.
fn percentile_25(mut values: Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    let k = values.len().div_ceil(4);
    values.get(k - 1).copied()
}

const EVENT_TYPES: &[&str] = &[
    "control_plane_tick",
    "spawn_gate_refused",
    "reign_checkin",
    "machine_sample",
];

#[derive(Debug, Clone)]
struct Point {
    ts: String,
    ts_ms: i64,
    value: f64,
}

#[derive(Debug, Clone)]
struct MemoryPoint {
    ts: String,
    ts_ms: i64,
    compressor_gb: f64,
    swap_gb: f64,
}

#[derive(Default)]
struct SeriesCov {
    rows: u64,
    first: Option<String>,
    last: Option<String>,
}

impl SeriesCov {
    fn note(&mut self, ts: &str) {
        self.rows += 1;
        if self.first.is_none() {
            self.first = Some(ts.to_string());
        }
        self.last = Some(ts.to_string());
    }
}

#[derive(Default)]
struct EventPass {
    readings: Vec<Reading>,
    refusals: Vec<Point>,
    live: Vec<Point>,
    memory: Vec<MemoryPoint>,
    tick_rows: SeriesCov,
    refusal_rows: SeriesCov,
    live_rows: SeriesCov,
    memory_rows: SeriesCov,
    no_reading: u64,
    unrecognized: u64,
    skipped_no_ts: u64,
    samples: Vec<SamplePoint>,
}

#[derive(Clone, Default)]
struct SamplePoint {
    ts: String,
    ts_ms: i64,
    top_rss: Vec<Value>,
    sessions: Vec<Value>,
    kings: Option<f64>,
    workers: Option<f64>,
    usable: bool,
}

/// One pass over both journals. Dedup by exact line text across the pair:
/// every daemon tick is mirrored to the global file, so the second sight of
/// a line is the same row, not a new measurement.
fn read_events(home: &AgentsHome) -> EventPass {
    let mut pass = EventPass::default();
    let mut seen: HashSet<String> = HashSet::new();
    for journal in [home.events_jsonl(), crate::daemon::global_events_path(home)] {
        let text = crate::events_store::journal_text(&journal, EVENT_TYPES);
        for line in text.lines() {
            if !seen.insert(line.to_string()) {
                continue;
            }
            read_event_line(line, &mut pass);
        }
    }
    pass
}

fn read_event_line(line: &str, pass: &mut EventPass) {
    let Ok(row) = serde_json::from_str::<Value>(line) else {
        return;
    };
    let row_type = row
        .get("type")
        .and_then(|v| v.as_str())
        .or_else(|| row.get("kind").and_then(|v| v.as_str()))
        .unwrap_or("");
    let ts = row.get("ts").and_then(|v| v.as_str()).unwrap_or("");
    let Ok(parsed) = DateTime::parse_from_rfc3339(ts) else {
        if !row_type.is_empty() {
            pass.skipped_no_ts += 1;
        }
        return;
    };
    let ts = parsed.with_timezone(&Utc);
    let ts_ms = parsed.timestamp_millis();
    let ts_string = ts.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let data = row.get("data").cloned().unwrap_or(Value::Null);
    match row_type {
        "control_plane_tick" => {
            if data.get("arm").and_then(|v| v.as_str()) != Some("machine_watch") {
                return;
            }
            pass.tick_rows.note(&ts_string);
            let detail = data.get("detail").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(reading) = parse_reading(detail, ts_ms, &ts_string) {
                pass.readings.push(reading);
            } else if data.get("skip_reason").and_then(|v| v.as_str()) == Some("machine_unreadable")
                || !detail.contains("band")
            {
                pass.no_reading += 1;
            } else {
                pass.unrecognized += 1;
            }
        }
        "spawn_gate_refused" => {
            pass.refusal_rows.note(&ts_string);
            pass.refusals.push(Point {
                ts: ts_string.clone(),
                ts_ms,
                value: 1.0,
            });
        }
        "reign_checkin" => {
            if let Some(live) = data.get("live_workers").and_then(|v| v.as_f64()) {
                pass.live_rows.note(&ts_string);
                pass.live.push(Point {
                    ts: ts_string.clone(),
                    ts_ms,
                    value: live,
                });
            }
        }
        "machine_sample" => {
            pass.samples.push(SamplePoint {
                ts: ts_string.clone(),
                ts_ms,
                top_rss: data
                    .get("top_rss")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default(),
                sessions: data
                    .get("sessions")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default(),
                kings: data.get("kings").and_then(|v| v.as_f64()),
                workers: data.get("workers").and_then(|v| v.as_f64()),
                usable: data.get("sessions").and_then(|v| v.as_array()).is_some(),
            });
            let compressor = data.get("compressor_gb").and_then(|v| v.as_f64());
            let swap = data.get("swap_used_gb").and_then(|v| v.as_f64());
            if let (Some(c), Some(s)) = (compressor, swap) {
                pass.memory_rows.note(&ts_string);
                pass.memory.push(MemoryPoint {
                    ts: ts_string.clone(),
                    ts_ms,
                    compressor_gb: c,
                    swap_gb: s,
                });
            }
        }
        _ => {}
    }
}

/// The whole report; group 2 renders the same shape as html.
#[derive(Debug, Default, Serialize)]
pub struct FleetReport {
    pub window_days: u64,
    pub floor: String,
    pub now: NowLine,
    pub hours: Vec<HourRow>,
    pub slowdowns: Vec<SlowdownRow>,
    pub threshold: Option<f64>,
    pub threshold_reason: String,
    pub curve: Option<Vec<Bucket>>,
    pub curve_reason: String,
    pub cap: Option<u64>,
    pub cap_reason: String,
    pub coverage: Coverage,
    pub holders: Vec<HolderRow>,
    pub stages: Vec<StageRow>,
    pub shape: FleetShape,
}

#[derive(Debug, Clone, Serialize)]
pub struct HolderRow {
    pub name: String,
    pub latest_rss_mb: f64,
    pub max_rss_mb: f64,
    pub max_at: String,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StageRow {
    pub stage: String,
    pub sessions_median: f64,
    pub rss_mb_median: f64,
    pub cpu_pct_median: f64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct FleetShape {
    pub kings_median: Option<f64>,
    pub workers_median: Option<f64>,
    pub king_ratio: Option<f64>,
    pub king_ratio_review: Option<bool>,
    pub merges: Option<u64>,
    pub worker_seats_mean: Option<f64>,
    pub merges_per_worker_seat_day: Option<f64>,
    pub reason: String,
}

#[derive(Debug, Default, Serialize)]
pub struct NowLine {
    pub reading: Option<Reading>,
    pub available_ram_gb: Option<f64>,
    pub swap_used_pct: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct HourRow {
    pub hour: String,
    pub claude: u64,
    pub subagent: u64,
    pub codex: u64,
    pub cargo: u64,
    pub pytest: u64,
    pub refusals: u64,
    pub load_15m_median: Option<f64>,
    pub busy_median: Option<f64>,
    pub compressor_gb_median: Option<f64>,
    pub swap_gb_median: Option<f64>,
    pub live_workers_last: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct SlowdownRow {
    pub ts: String,
    pub session: String,
    pub harness: String,
    pub text: String,
    /// The nearest reading within 60 minutes, when one exists.
    pub reading: Option<NearReading>,
}

#[derive(Debug, Serialize)]
pub struct NearReading {
    pub load_15m: Option<f64>,
    pub busy_pct: f64,
    pub offset_min: i64,
}

#[derive(Debug, Serialize)]
pub struct Bucket {
    pub label: String,
    pub top: u64,
    pub hours: u64,
    pub load_15m_median: Option<f64>,
    /// Share of this bucket's hours at or above the threshold (0..1), when
    /// a threshold exists.
    pub over_share: Option<f64>,
    pub compressor_gb_median: Option<f64>,
    pub swap_gb_median: Option<f64>,
}

#[derive(Debug, Default, Serialize)]
pub struct Coverage {
    pub readings: SeriesCoverage,
    pub refusals: SeriesCoverage,
    pub live: SeriesCoverage,
    pub memory: SeriesCoverage,
    pub no_reading: u64,
    pub unrecognized: u64,
    pub skipped_no_ts: u64,
    pub journals: Vec<String>,
    pub(crate) fold: FoldReceipt,
}

#[derive(Debug, Default, Serialize)]
pub struct SeriesCoverage {
    pub rows: u64,
    pub first: Option<String>,
    pub last: Option<String>,
}

pub(crate) struct Inputs {
    pub home: AgentsHome,
    pub state_dir: PathBuf,
    pub roots: Roots,
    pub now: DateTime<Utc>,
    pub window_days: u64,
    pub budget: Option<u64>,
}

/// The engine: events + the incremental fold, assembled into the report.
pub(crate) fn analyze(inputs: &Inputs) -> FleetReport {
    let pass = read_events(&inputs.home);
    let cache = inputs.state_dir.join("fleet").join("activity.json");
    let (activity, receipt) = crate::transcript_activity::fold(
        &cache,
        &inputs.roots,
        inputs.now,
        inputs.window_days,
        inputs.budget,
    )
    .unwrap_or_else(|_| (Activity::default(), FoldReceipt::default()));
    let floor_dt = inputs.now - chrono::Duration::days(inputs.window_days as i64);
    let floor = hour_string(floor_dt);
    let floor_ms = floor_dt.timestamp_millis();
    let now_hour = hour_string(inputs.now);

    // Hours: one row per UTC hour with activity or a reading, windowed.
    let mut hours: BTreeMap<String, HourRow> = BTreeMap::new();
    fn hour_of(map: &mut BTreeMap<String, HourRow>, hour: String) -> &mut HourRow {
        map.entry(hour.clone()).or_insert_with(|| HourRow {
            hour,
            claude: 0,
            subagent: 0,
            codex: 0,
            cargo: 0,
            pytest: 0,
            refusals: 0,
            load_15m_median: None,
            busy_median: None,
            compressor_gb_median: None,
            swap_gb_median: None,
            live_workers_last: None,
        })
    }
    for (hour, counts) in &activity.hours {
        if hour.as_str() < floor.as_str() || hour.as_str() > now_hour.as_str() {
            continue;
        }
        let row = hour_of(&mut hours, hour.clone());
        row.claude = counts.claude;
        row.subagent = counts.subagent;
        row.codex = counts.codex;
        row.cargo = counts.cargo;
        row.pytest = counts.pytest;
    }
    let now_ms = inputs.now.timestamp_millis();
    let mut refusals_per_hour: HashMap<String, u64> = HashMap::new();
    for p in &pass.refusals {
        if p.ts_ms < floor_ms || p.ts_ms > now_ms {
            continue;
        }
        let hour = &p.ts[..13];
        *refusals_per_hour.entry(hour.to_string()).or_insert(0) += 1;
    }
    for (hour, n) in &refusals_per_hour {
        hour_of(&mut hours, hour.clone()).refusals = *n;
    }
    // Per-hour medians for the reading and memory series.
    let mut loads_per_hour: HashMap<String, Vec<f64>> = HashMap::new();
    let mut busy_per_hour: HashMap<String, Vec<f64>> = HashMap::new();
    for r in &pass.readings {
        if r.ts_ms < floor_ms || r.ts_ms > now_ms {
            continue;
        }
        // An unavailable load never invents a zero: the hour's busy median
        // still records, but the reading is omitted from the load samples.
        if let Some(load) = r.load_15m {
            loads_per_hour
                .entry(r.ts[..13].to_string())
                .or_default()
                .push(load);
        }
        busy_per_hour
            .entry(r.ts[..13].to_string())
            .or_default()
            .push(r.busy_pct);
    }
    for (hour, mut values) in loads_per_hour {
        hour_of(&mut hours, hour).load_15m_median = median(&mut values);
    }
    for (hour, mut values) in busy_per_hour {
        hour_of(&mut hours, hour).busy_median = median(&mut values);
    }
    let mut mem_per_hour: HashMap<String, Vec<MemoryPoint>> = HashMap::new();
    for m in &pass.memory {
        if m.ts_ms < floor_ms || m.ts_ms > now_ms {
            continue;
        }
        mem_per_hour
            .entry(m.ts[..13].to_string())
            .or_default()
            .push(m.clone());
    }
    for (hour, samples) in mem_per_hour {
        let row = hour_of(&mut hours, hour);
        let mut c: Vec<f64> = samples.iter().map(|m| m.compressor_gb).collect();
        let mut s: Vec<f64> = samples.iter().map(|m| m.swap_gb).collect();
        row.compressor_gb_median = median(&mut c);
        row.swap_gb_median = median(&mut s);
    }
    for p in &pass.live {
        if p.ts_ms < floor_ms || p.ts_ms > now_ms {
            continue;
        }
        let row = hour_of(&mut hours, p.ts[..13].to_string());
        row.live_workers_last = Some(p.value);
    }
    let mut hour_rows: Vec<HourRow> = hours.into_values().collect();
    hour_rows.retain(|row| {
        row.claude + row.subagent + row.codex + row.cargo + row.pytest + row.refusals > 0
            || row.load_15m_median.is_some()
    });
    hour_rows.sort_by(|a, b| a.hour.cmp(&b.hour));

    // The windowed readings feed every nearest-reading join: a cache or
    // journal row older than the floor never leaks into threshold, cap or
    // the slowdown table.
    let windowed: Vec<&Reading> = pass
        .readings
        .iter()
        .filter(|r| r.ts_ms >= floor_ms && r.ts_ms <= now_ms)
        .collect();

    let mut report = FleetReport {
        window_days: inputs.window_days,
        threshold_reason: String::new(),
        curve_reason: String::new(),
        cap_reason: String::new(),
        now: now_line(&pass),
        hours: hour_rows,
        slowdowns: slowdown_rows(&activity.slowdowns, &windowed, floor_ms),
        threshold: None,
        curve: None,
        cap: None,
        coverage: Coverage {
            readings: cov_of(&pass.tick_rows),
            refusals: cov_of(&pass.refusal_rows),
            live: cov_of(&pass.live_rows),
            memory: cov_of(&pass.memory_rows),
            no_reading: pass.no_reading,
            unrecognized: pass.unrecognized,
            skipped_no_ts: pass.skipped_no_ts,
            journals: vec![
                inputs.home.events_jsonl().display().to_string(),
                crate::daemon::global_events_path(&inputs.home)
                    .display()
                    .to_string(),
            ],
            fold: receipt,
        },
        floor,
        holders: Vec::new(),
        stages: Vec::new(),
        shape: FleetShape::default(),
    };
    summarize_samples(
        &mut report,
        &pass.samples,
        floor_ms,
        now_ms,
        inputs.window_days,
    );
    with_threshold_curve_cap(
        &mut report,
        &windowed,
        &activity.slowdowns,
        &now_hour,
        floor_ms,
    );
    report
}

fn median_f64(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(values[values.len() / 2])
}

fn summarize_samples(
    report: &mut FleetReport,
    samples: &[SamplePoint],
    floor_ms: i64,
    now_ms: i64,
    window_days: u64,
) {
    let samples: Vec<&SamplePoint> = samples
        .iter()
        .filter(|sample| sample.usable && sample.ts_ms >= floor_ms && sample.ts_ms <= now_ms)
        .collect();
    if samples.is_empty() {
        report.shape.reason = "no machine_sample rows in this window".into();
        return;
    }
    let mut holders: BTreeMap<String, HolderRow> = BTreeMap::new();
    for sample in &samples {
        for holder in &sample.top_rss {
            let Some(name) = holder.get("name").and_then(Value::as_str) else {
                continue;
            };
            let rss = holder.get("rss_mb").and_then(Value::as_f64).unwrap_or(0.0);
            let entry = holders.entry(name.into()).or_insert_with(|| HolderRow {
                name: name.into(),
                latest_rss_mb: rss,
                max_rss_mb: rss,
                max_at: sample.ts.clone(),
                session_id: holder
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
            entry.latest_rss_mb = rss;
            if rss >= entry.max_rss_mb {
                entry.max_rss_mb = rss;
                entry.max_at = sample.ts.clone();
            }
        }
    }
    report.holders = holders.into_values().collect();
    report.holders.sort_by(|a, b| {
        b.max_rss_mb
            .partial_cmp(&a.max_rss_mb)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    report.holders.truncate(10);

    let mut stage_values: BTreeMap<String, Vec<(f64, f64, f64)>> = BTreeMap::new();
    for sample in &samples {
        let mut by_stage: BTreeMap<String, (f64, f64, f64)> = BTreeMap::new();
        for session in &sample.sessions {
            let stage = session
                .get("stage")
                .and_then(Value::as_str)
                .unwrap_or("unstaged")
                .to_string();
            let entry = by_stage.entry(stage).or_default();
            entry.0 += 1.0;
            entry.1 += session.get("rss_mb").and_then(Value::as_f64).unwrap_or(0.0);
            entry.2 += session
                .get("cpu_pct")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
        }
        for (stage, values) in by_stage {
            stage_values.entry(stage).or_default().push(values);
        }
    }
    for (stage, values) in stage_values {
        let mut sessions: Vec<f64> = values.iter().map(|v| v.0).collect();
        let mut rss: Vec<f64> = values.iter().map(|v| v.1).collect();
        let mut cpu: Vec<f64> = values.iter().map(|v| v.2).collect();
        report.stages.push(StageRow {
            stage,
            sessions_median: median_f64(&mut sessions).unwrap_or(0.0),
            rss_mb_median: median_f64(&mut rss).unwrap_or(0.0),
            cpu_pct_median: median_f64(&mut cpu).unwrap_or(0.0),
        });
    }
    let mut kings: Vec<f64> = samples.iter().filter_map(|s| s.kings).collect();
    let mut workers: Vec<f64> = samples.iter().filter_map(|s| s.workers).collect();
    let kings_median = median_f64(&mut kings);
    let workers_median = median_f64(&mut workers);
    report.shape.kings_median = kings_median;
    report.shape.workers_median = workers_median;
    report.shape.king_ratio = kings_median
        .zip(workers_median)
        .and_then(|(k, w)| (w > 0.0).then_some(k / w));
    report.shape.king_ratio_review = kings_median.zip(workers_median).map(|(k, w)| k * 4.0 > w);
    report.shape.worker_seats_mean = if workers.is_empty() {
        None
    } else {
        Some(workers.iter().sum::<f64>() / workers.len() as f64)
    };
    report.shape.reason = format!("{} sample rows", samples.len());
    let _ = window_days;
}

/// Threshold from the slowdown nearest-readings, then the capacity curve and
/// the cap. While the fold receipt reports pending bytes, none are computed:
/// a transcript not read yet would put a loaded hour in a low bucket. The
/// reasons say which gate fired.
fn with_threshold_curve_cap(
    report: &mut FleetReport,
    readings: &[&Reading],
    slowdowns: &[Slowdown],
    now_hour: &str,
    floor_ms: i64,
) {
    let pending = report.coverage.fold.pending_bytes;
    // Snapshot the curve inputs first: the hours borrow report immutably
    // while every later step writes reasons, curve, threshold and cap
    // through &mut.
    let hour_snaps: Vec<HourSnap> = report
        .hours
        .iter()
        .filter(|h| h.hour != now_hour)
        .map(|h| HourSnap {
            sessions: h.claude + h.subagent + h.codex,
            load: h.load_15m_median,
            compressor: h.compressor_gb_median,
            swap: h.swap_gb_median,
        })
        .collect();
    if pending > 0 {
        let why = format!(
            "backfill incomplete: {} file(s), {} bytes unread; rerun with --backfill",
            report.coverage.fold.pending_files, pending
        );
        report.threshold_reason = why.clone();
        report.curve_reason = why.clone();
        report.cap_reason = why;
        return;
    }
    // Threshold: the 25th percentile, nearest rank, of the load_15m at the
    // slowdown turns of THIS window that have a reading in it. Needs at
    // least 2. A cache from a wider window must not leak turns in.
    let mut at_slowdowns: Vec<f64> = Vec::new();
    for s in slowdowns {
        let Ok(ts) = DateTime::parse_from_rfc3339(&s.ts) else {
            continue;
        };
        let ms = ts.timestamp_millis();
        if ms < floor_ms {
            continue;
        }
        if let Some(Some((Some(load), _, _))) = nearest_reading(readings, ms) {
            at_slowdowns.push(load);
        }
    }
    let n_with_reading = at_slowdowns.len();
    if n_with_reading < 2 {
        let why = format!("{n_with_reading} slowdown turns with a reading, 2 needed");
        report.threshold_reason = why.clone();
        report.cap_reason = why;
        set_curve(report, &hour_snaps, None);
        return;
    }
    let Some(threshold) = percentile_25(at_slowdowns) else {
        return;
    };
    report.threshold = Some(threshold);
    report.threshold_reason = format!(
        "25th percentile of the load_15m at {n_with_reading} slowdown turns with a reading"
    );
    set_curve(report, &hour_snaps, Some(threshold));
    // Cap: walk the buckets upward, skipping buckets with fewer than 6
    // hours. The cap is the top of the last bucket whose median stays under
    // the threshold before the first bucket at or above it.
    let buckets = report.curve.as_ref().map(|c| c.as_slice()).unwrap_or(&[]);
    let qualifying: Vec<&Bucket> = buckets.iter().filter(|b| b.hours >= 6).collect();
    if qualifying.is_empty() {
        report.cap_reason = "no bucket holds 6 hours or more".to_string();
        return;
    }
    let over = qualifying
        .iter()
        .position(|b| b.load_15m_median.is_some_and(|m| m >= threshold));
    match over {
        Some(0) => {
            report.cap_reason =
                "the lowest qualifying bucket already sits at or above the threshold".to_string();
        }
        Some(i) => {
            let prev = qualifying[i - 1];
            report.cap = Some(prev_top(prev));
            report.cap_reason = format!(
                "the {} bucket's median load stays under the threshold; the next bucket at or above it is {}",
                prev.label, qualifying[i].label
            );
        }
        None => {
            let last = qualifying[qualifying.len() - 1];
            report.cap = Some(prev_top(last));
            report.cap_reason = format!(
                "no bucket at or above the threshold; the top of the {} bucket is the observed ceiling",
                last.label
            );
        }
    }
}

fn prev_top(b: &Bucket) -> u64 {
    b.top
}

fn set_curve(report: &mut FleetReport, hours: &[HourSnap], threshold: Option<f64>) {
    // Bucket complete hours with a reading by sessions active. Buckets of
    // 10; the current hour is left out, its sessions still counting.
    let mut groups: BTreeMap<u64, Vec<HourSnap>> = BTreeMap::new();
    for h in hours {
        if h.load.is_none() {
            continue;
        }
        groups.entry(h.sessions / 10).or_default().push(*h);
    }
    if groups.is_empty() {
        report.curve_reason = "no machine_watch readings in the window".to_string();
        return;
    }
    let mut out: Vec<Bucket> = Vec::new();
    for (idx, group) in groups {
        let mut loads: Vec<f64> = group.iter().filter_map(|h| h.load).collect();
        let over = threshold.map(|t| {
            group
                .iter()
                .filter(|h| h.load.is_some_and(|v| v >= t))
                .count() as f64
                / group.len() as f64
        });
        let mut comps: Vec<f64> = group.iter().filter_map(|h| h.compressor).collect();
        let mut swaps: Vec<f64> = group.iter().filter_map(|h| h.swap).collect();
        out.push(Bucket {
            label: format!("{}-{}", idx * 10, idx * 10 + 9),
            top: idx * 10 + 9,
            hours: group.len() as u64,
            load_15m_median: median(&mut loads),
            over_share: over,
            compressor_gb_median: median(&mut comps),
            swap_gb_median: median(&mut swaps),
        });
    }
    report.curve = Some(out);
}

#[derive(Debug, Clone, Copy)]
struct HourSnap {
    sessions: u64,
    load: Option<f64>,
    compressor: Option<f64>,
    swap: Option<f64>,
}

/// The reading nearest in time to `ms`, when it sits within 60 minutes.
fn nearest_reading(readings: &[&Reading], ms: i64) -> Option<Option<(Option<f64>, f64, i64)>> {
    let mut best: Option<&Reading> = None;
    for r in readings {
        let d = (r.ts_ms - ms).abs();
        if d <= 60 * 60 * 1000 && best.is_none_or(|b| (b.ts_ms - ms).abs() > d) {
            best = Some(r);
        }
    }
    best.map(|r| Some((r.load_15m, r.busy_pct, (r.ts_ms - ms) / 60_000)))
}

fn now_line(pass: &EventPass) -> NowLine {
    let reading = pass.readings.iter().max_by_key(|r| r.ts_ms).cloned();
    NowLine {
        reading,
        available_ram_gb: crate::spawn_gate::available_ram_gb(),
        swap_used_pct: crate::spawn_gate::swap_used_pct(),
    }
}

fn slowdown_rows(all: &[Slowdown], readings: &[&Reading], floor_ms: i64) -> Vec<SlowdownRow> {
    let mut rows: Vec<SlowdownRow> = Vec::new();
    for s in all {
        let Ok(ts) = DateTime::parse_from_rfc3339(&s.ts) else {
            continue;
        };
        let ms = ts.timestamp_millis();
        if ms < floor_ms {
            continue;
        }
        let reading =
            nearest_reading(readings, ms)
                .flatten()
                .map(|(load_15m, busy_pct, offset_min)| NearReading {
                    load_15m,
                    busy_pct,
                    offset_min,
                });
        rows.push(SlowdownRow {
            ts: s.ts.clone(),
            session: s.session.clone(),
            harness: s.harness.clone(),
            text: s.text.clone(),
            reading,
        });
    }
    rows.sort_by(|a, b| a.ts.cmp(&b.ts));
    rows
}

fn cov_of(cov: &SeriesCov) -> SeriesCoverage {
    SeriesCoverage {
        rows: cov.rows,
        first: cov.first.clone(),
        last: cov.last.clone(),
    }
}

/// The on-demand verb: an argument of the existing `fno-agents intel` fold,
/// never a new action.
pub fn run_fleet_cli(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!(
            "fno-agents intel --fleet [--days N] [--backfill] [--json|--html] [--out PATH]\n\n\
             The fleet capacity fold: the machine's load history beside what the\n\
             fleet was running, a slowdown threshold from the operator's own\n\
             turns, and a suggested session cap. Default window 30 days;\n\
             --backfill lifts the per-run read budget so the first run reads\n\
             every transcript in the window. Exit 0 when a report printed.\n"
        );
        return 0;
    }
    let mut days = DEFAULT_WINDOW_DAYS;
    let mut backfill = false;
    let mut json = false;
    let mut html = false;
    let mut out: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--fleet" => {}
            "--days" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse::<u64>().ok()) {
                    Some(0) => {
                        eprintln!(
                            "fno-agents intel --fleet: --days 0 is not supported here; the window must be at least 1 day"
                        );
                        return 2;
                    }
                    Some(v) => days = v,
                    None => {
                        eprintln!("fno-agents intel: --days needs a non-negative integer");
                        return 2;
                    }
                }
            }
            "--backfill" => backfill = true,
            "--json" | "-J" => json = true,
            "--html" => html = true,
            "--out" => {
                i += 1;
                match args.get(i) {
                    Some(path) => out = Some(PathBuf::from(path)),
                    None => {
                        eprintln!("fno-agents intel --fleet: --out needs a path");
                        return 2;
                    }
                }
            }
            other => {
                eprintln!("fno-agents intel: unknown flag {other}");
                return 2;
            }
        }
        i += 1;
    }
    if html && json {
        eprintln!("fno-agents intel --fleet: --html and --json are mutually exclusive");
        return 2;
    }
    if out.is_some() && !html {
        eprintln!("fno-agents intel --fleet: --out requires --html");
        return 2;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let Some(state_dir) = crate::agents_config::state_dir(&cwd) else {
        eprintln!("fno-agents intel --fleet: no state dir resolves from this directory");
        return 1;
    };
    let home = AgentsHome::from_env();
    let inputs = Inputs {
        home,
        roots: Roots {
            claude_projects: crate::claude_drive::claude_projects_dir(),
            codex_sessions: None,
            bus_log: crate::intel::bus_log_path(&state_dir),
        },
        state_dir: state_dir.clone(),
        now: Utc::now(),
        window_days: days,
        budget: if backfill {
            None
        } else {
            Some(TICK_READ_BUDGET)
        },
    };
    let report = analyze(&inputs);
    if html {
        let path = out.unwrap_or_else(|| state_dir.join("fleet.html"));
        let reload_s = crate::king_ledger::reload_secs(crate::agents_config::config_lookup(
            &cwd,
            &["backlog", "page_reload_s"],
        ));
        let body = crate::fleet_page::render(&report, &Utc::now().to_rfc3339(), reload_s);
        if let Err(error) = crate::fleet_page::write_page(&path, &body) {
            eprintln!("fno-agents intel --fleet --html: {error}");
            return 1;
        }
        println!("{}", path.display());
        return 0;
    }
    let text = if json {
        serde_json::to_string(&report).unwrap_or_else(|_| "{}".to_string())
    } else {
        render_text(&report)
    };
    println!("{text}");
    0
}

/// The terminal report. Every section states what is missing and why when it
/// is empty; a fresh install prints honest empty states end to end.
pub(crate) fn render_text(report: &FleetReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "fleet load, last {} days ({} .. {} UTC)\n",
        report.window_days,
        report.floor,
        report
            .now
            .reading
            .as_ref()
            .map(|r| r.ts[..16].to_string())
            .unwrap_or_else(|| "now".to_string()),
    ));
    // now
    match &report.now.reading {
        Some(r) => {
            let load = r
                .load_15m
                .map(|l| format!("{l:.1}"))
                .unwrap_or_else(|| "unavailable".to_string());
            out.push_str(&format!(
                "now: {} busy {:.1}%, load_15m {}, {} runnable of {} processes; ram {} gb available, swap {}% used\n",
                r.verdict,
                r.busy_pct,
                load,
                r.runnable.map(|v| v.to_string()).unwrap_or_else(|| "?".into()),
                r.processes.map(|v| v.to_string()).unwrap_or_else(|| "?".into()),
                report
                    .now
                    .available_ram_gb
                    .map(|v| format!("{v:.1}"))
                    .unwrap_or_else(|| "unreadable".into()),
                report
                    .now
                    .swap_used_pct
                    .map(|v| format!("{v:.0}"))
                    .unwrap_or_else(|| "unreadable".into()),
            ));
        }
        None => out.push_str(&format!(
            "now: no machine_watch reading yet; ram {}, swap {}\n",
            report
                .now
                .available_ram_gb
                .map(|v| format!("{v:.1} gb available"))
                .unwrap_or_else(|| "unreadable".into()),
            report
                .now
                .swap_used_pct
                .map(|v| format!("{v:.0}% used"))
                .unwrap_or_else(|| "unreadable".into()),
        )),
    }
    // capacity curve
    match &report.curve {
        Some(buckets) => {
            out.push_str(
                "capacity curve, hours by sessions active (claude + subagents + codex):\n",
            );
            out.push_str(
                "  sessions   hours  median load_15m  at-or-above  compressor gb  swap gb\n",
            );
            for b in buckets {
                out.push_str(&format!(
                    "  {:>8}  {:>5}  {:>15}  {:>12}  {:>13}  {:>7}\n",
                    b.label,
                    b.hours,
                    b.load_15m_median
                        .map(|v| format!("{v:.1}"))
                        .unwrap_or_else(|| "-".into()),
                    b.over_share
                        .map(|v| format!("{:.0}%", v * 100.0))
                        .unwrap_or_else(|| "-".into()),
                    b.compressor_gb_median
                        .map(|v| format!("{v:.1}"))
                        .unwrap_or_else(|| "-".into()),
                    b.swap_gb_median
                        .map(|v| format!("{v:.1}"))
                        .unwrap_or_else(|| "-".into()),
                ));
            }
        }
        None => out.push_str(&format!("no capacity curve: {}\n", report.curve_reason)),
    }
    // threshold
    match report.threshold {
        Some(t) => out.push_str(&format!(
            "threshold: load_15m {:.1} ({})\n",
            t, report.threshold_reason
        )),
        None => out.push_str(&format!("no threshold: {}\n", report.threshold_reason)),
    }
    // cap
    match report.cap {
        Some(c) => out.push_str(&format!(
            "suggested cap: {} sessions active (claude + subagents + codex per hour; not spawn-gate worker rows)\n",
            c
        )),
        None => out.push_str(&format!("no suggested cap: {}\n", report.cap_reason)),
    }
    // slowdowns
    if report.slowdowns.is_empty() {
        out.push_str("no slowdown turns in the window\n");
    } else {
        out.push_str(&format!(
            "slowdown turns ({} in window, last 10):\n",
            report.slowdowns.len()
        ));
        for s in report.slowdowns.iter().rev().take(10) {
            let near = match &s.reading {
                Some(n) => format!(
                    "load_15m {}, busy {:.1}% at {} min",
                    n.load_15m
                        .map(|v| format!("{v:.1}"))
                        .unwrap_or_else(|| "?".into()),
                    n.busy_pct,
                    n.offset_min
                ),
                None => "no reading within an hour".to_string(),
            };
            out.push_str(&format!(
                "  {} {} [{}] \"{}\" ({})\n",
                s.ts, s.session, s.harness, s.text, near
            ));
        }
    }
    // coverage
    let cov = &report.coverage;
    out.push_str("coverage:\n");
    out.push_str(&format!(
        "  machine_watch: {} reading rows {}..{}\n",
        cov.readings.rows,
        cov.readings.first.as_deref().unwrap_or("-"),
        cov.readings.last.as_deref().unwrap_or("-"),
    ));
    out.push_str(&format!(
        "  gate refusals: {} rows {}..{}\n",
        cov.refusals.rows,
        cov.refusals.first.as_deref().unwrap_or("-"),
        cov.refusals.last.as_deref().unwrap_or("-"),
    ));
    out.push_str(&format!(
        "  live workers: {} points {}..{}\n",
        cov.live.rows,
        cov.live.first.as_deref().unwrap_or("-"),
        cov.live.last.as_deref().unwrap_or("-"),
    ));
    out.push_str(&format!(
        "  memory samples: {} rows {}..{} (structured machine_sample rows)\n",
        cov.memory.rows,
        cov.memory.first.as_deref().unwrap_or("-"),
        cov.memory.last.as_deref().unwrap_or("-"),
    ));
    if report.holders.is_empty() {
        out.push_str("no top memory holders: no machine_sample rows in this window\n");
    } else {
        out.push_str("top memory holders:\n");
        for holder in &report.holders {
            out.push_str(&format!(
                "  {} latest {:.1} MB max {:.1} MB at {}\n",
                holder.name, holder.latest_rss_mb, holder.max_rss_mb, holder.max_at
            ));
        }
    }
    if report.stages.is_empty() {
        out.push_str("no cost by stage: session costs unavailable\n");
    } else {
        out.push_str("cost by stage:\n");
        for stage in &report.stages {
            out.push_str(&format!(
                "  {}: {:.1} sessions, {:.1} MB RSS, {:.1}% CPU\n",
                stage.stage, stage.sessions_median, stage.rss_mb_median, stage.cpu_pct_median
            ));
        }
    }
    match (report.shape.king_ratio, report.shape.worker_seats_mean) {
        (Some(ratio), Some(seats)) => out.push_str(&format!(
            "fleet shape: {:.2} kings per worker seat; {:.1} seats; review={}\n",
            ratio,
            seats,
            report.shape.king_ratio_review.unwrap_or(false)
        )),
        _ => out.push_str(&format!("fleet shape: {}\n", report.shape.reason)),
    }
    out.push_str(&format!(
        "  ticks unparsed: {} no-reading, {} unrecognized, {} skipped (no ts)\n",
        cov.no_reading, cov.unrecognized, cov.skipped_no_ts
    ));
    out.push_str(&format!(
        "  transcripts: {} files read, {} pending ({} bytes), {} replaced{}\n",
        cov.fold.files_read,
        cov.fold.pending_files,
        cov.fold.pending_bytes,
        cov.fold.replaced,
        cov.fold
            .cache_reset
            .as_ref()
            .map(|w| format!("; cache reset: {w}"))
            .unwrap_or_default(),
    ));
    out.push_str(&format!(
        "  journals: {}\n  backfill floor: {}Z (window {} days)\n",
        cov.journals.join(", "),
        report.floor,
        report.window_days
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::path::Path;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Pins FNO_AGENTS_HOME for one AgentsHome::from_env() read.
    fn pinned_home(dir: &Path) -> AgentsHome {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: single-threaded relative to other env reads via ENV_LOCK.
        std::env::set_var("FNO_AGENTS_HOME", dir.join("fno").join("agents"));
        AgentsHome::from_env()
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("fno-fleet-load-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn tick(ts: &str, load: &str) -> String {
        format!(
            "{{\"ts\":\"{ts}\",\"type\":\"control_plane_tick\",\"source\":\"loop\",\"data\":{{\"arm\":\"machine_watch\",\"scheduler\":\"daemon\",\"acted\":1,\"interval_s\":300,\"skip_reason\":null,\"detail\":\"machine 50.0% of band 90% (6.0 of 12.00 cores) -> calm; load_15m {load}, 4 runnable of 662 processes\"}}}}\n"
        )
    }

    fn write_lines(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    fn roots(dir: &Path) -> Roots {
        Roots {
            claude_projects: dir.join("projects"),
            codex_sessions: Some(dir.join("codex")),
            bus_log: dir.join("bus/messages.jsonl"),
        }
    }

    fn hour_n(n: u32) -> String {
        // Hour n from 2026-08-30T00 (UTC); hours 0-29 predate the test's now.
        let base = Utc.with_ymd_and_hms(2026, 8, 30, 0, 0, 0).unwrap();
        let dt = base + chrono::Duration::hours(n as i64);
        dt.format("%Y-%m-%dT%H").to_string()
    }

    fn stamp(hour: u32, minute: u32) -> String {
        let base = Utc.with_ymd_and_hms(2026, 8, 30, 0, 0, 0).unwrap();
        let dt =
            base + chrono::Duration::hours(hour as i64) + chrono::Duration::minutes(minute as i64);
        dt.format("%Y-%m-%dT%H:%M:00Z").to_string()
    }

    fn claude_user(ts: &str, text: &str) -> String {
        format!(
            "{{\"type\":\"user\",\"timestamp\":\"{ts}\",\"message\":{{\"content\":\"{text}\"}}}}\n"
        )
    }

    /// The curve corpus: 30 hours. Hours 0-9 hold 5 active sessions at load
    /// 50, hours 10-19 hold 15 at 100, hours 20-29 hold 25 at 250, plus two
    /// slowdown turns whose nearest readings read 240 and 260. One extra
    /// tick sits in the current hour when the test's `now` reads 2026-09-02,
    /// and never enters a bucket.
    fn build_curve_fixture(tag: &str) -> PathBuf {
        let dir = tmp_dir(tag);
        let home = dir.join("fno").join("agents");
        let mut journal = String::new();
        for h in 0..30u32 {
            let load = match h {
                0..=9 => "50.0",
                10..=19 => "100.0",
                _ => "250.0",
            };
            journal.push_str(&tick(&stamp(h, 5), load));
        }
        // Two ticks that make the slowdown nearest-readings read 240/260,
        // and one in the never-bucketed current hour (2026-09-02T00).
        journal.push_str(&tick(&stamp(2, 10), "240.0"));
        journal.push_str(&tick(&stamp(12, 15), "260.0"));
        journal.push_str(&tick(&stamp(72, 5), "900.0"));
        // Two operator slowdown turns.
        journal.push_str(&format!(
            "{{\"ts\":\"{}\",\"kind\":\"spawn_gate_refused\",\"data\":{{}}}}\n",
            stamp(5, 20)
        ));
        write_lines(&home.join("events.jsonl"), &journal);
        // Transcripts: 5, 15 and 25 claude files, one active hour per group.
        let projects = dir.join("projects").join("-proj");
        for g in 0..3u32 {
            for f in 0..(5 + g * 10) {
                let mut lines = String::new();
                for h in (g * 10)..(g * 10 + 10) {
                    lines.push_str(&claude_user(&stamp(h, 15), &format!("hello {f}")));
                }
                write_lines(
                    &projects.join(format!("{:02}-g{f}.jsonl", g * 10 + f)),
                    &lines,
                );
            }
        }
        // One transcript carrying both slowdown turns.
        write_lines(
            &projects.join("99-slow.jsonl"),
            &format!(
                "{}{}",
                claude_user(&stamp(2, 30), "everything is so slow"),
                claude_user(&stamp(12, 35), "everything is so slow")
            ),
        );
        dir
    }

    fn inputs(dir: &Path, home: AgentsHome, budget: Option<u64>) -> Inputs {
        Inputs {
            home,
            state_dir: dir.join("fno"),
            roots: roots(dir),
            now: Utc.with_ymd_and_hms(2026, 9, 2, 0, 0, 0).unwrap(),
            window_days: 30,
            budget,
        }
    }

    #[test]
    fn ac5_threshold_curve_cap_from_slowdowns() {
        let dir = build_curve_fixture("ac5");
        let home = pinned_home(&dir);
        let report = analyze(&inputs(&dir, home, None));
        assert_eq!(report.threshold, Some(240.0));
        let buckets = report.curve.as_ref().expect("curve computed");
        let b_2029 = buckets.iter().find(|b| b.label == "20-29").unwrap();
        let b_1019 = buckets.iter().find(|b| b.label == "10-19").unwrap();
        assert_eq!(b_2029.hours, 10);
        assert_eq!(b_1019.hours, 10);
        assert_eq!(b_2029.load_15m_median, Some(250.0));
        assert_eq!(b_1019.load_15m_median, Some(100.0));
        assert_eq!(report.cap, Some(19));
        assert!(
            report
                .cap_reason
                .contains("the 10-19 bucket's median load stays under the threshold"),
            "cap reason names the under bucket: {}",
            report.cap_reason
        );
        let text = render_text(&report);
        assert!(text.contains("suggested cap: 19 sessions active"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ac6_event_series_parse_from_both_row_shapes() {
        let dir = tmp_dir("ac6");
        let home_dir = dir.join("fno").join("agents");
        let mut journal = String::new();
        journal.push_str(&tick(&stamp(0, 5), "75.9")); // calm reading
                                                       // Hot reading: crosses band, prefixed by the escalation marker.
        journal.push_str(&format!(
            "{{\"ts\":\"{}\",\"type\":\"control_plane_tick\",\"source\":\"loop\",\"data\":{{\"arm\":\"machine_watch\",\"scheduler\":\"daemon\",\"acted\":1,\"interval_s\":300,\"skip_reason\":null,\"detail\":\"1/2 hot: machine 91.2% crosses band 90% (11.2 of 12.00 cores) -> hot; load_15m 120.4, 300 runnable of 662 processes\"}}}}\n",
            stamp(0, 12)
        ));
        // load_15m unavailable still parses as a reading without a load.
        journal.push_str(&format!(
            "{{\"ts\":\"{}\",\"type\":\"control_plane_tick\",\"source\":\"loop\",\"data\":{{\"arm\":\"machine_watch\",\"scheduler\":\"daemon\",\"acted\":1,\"interval_s\":300,\"skip_reason\":null,\"detail\":\"machine 50.0% of band 90% (6.0 of 12.00 cores) -> calm; load_15m unavailable, 4 runnable of 662 processes\"}}}}\n",
            stamp(0, 20)
        ));
        // machine_unreadable: no reading, counted.
        journal.push_str(&format!(
            "{{\"ts\":\"{}\",\"type\":\"control_plane_tick\",\"source\":\"loop\",\"data\":{{\"arm\":\"machine_watch\",\"scheduler\":\"daemon\",\"acted\":0,\"interval_s\":300,\"skip_reason\":\"machine_unreadable\",\"detail\":\"machine reading unavailable (ps failed)\"}}}}\n",
            stamp(0, 25)
        ));
        // Two flat refusals in one hour.
        journal.push_str(&format!(
            "{{\"ts\":\"{}\",\"kind\":\"spawn_gate_refused\",\"data\":{{}}}}\n",
            stamp(0, 30)
        ));
        journal.push_str(&format!(
            "{{\"ts\":\"{}\",\"kind\":\"spawn_gate_refused\",\"data\":{{}}}}\n",
            stamp(0, 40)
        ));
        // One live point, one memory point.
        journal.push_str(&format!(
            "{{\"ts\":\"{}\",\"type\":\"reign_checkin\",\"source\":\"loop\",\"data\":{{\"live_workers\":7}}}}\n",
            stamp(0, 45)
        ));
        journal.push_str(&format!(
            "{{\"ts\":\"{}\",\"type\":\"machine_sample\",\"source\":\"loop\",\"data\":{{\"compressor_gb\":2.5,\"swap_used_gb\":9.1}}}}\n",
            stamp(0, 50)
        ));
        write_lines(&home_dir.join("events.jsonl"), &journal);
        let home = pinned_home(&dir);
        let report = analyze(&inputs(&dir, home, None));
        assert_eq!(
            report.coverage.readings.rows, 4,
            "all machine_watch rows read"
        );
        assert_eq!(report.coverage.no_reading, 1);
        assert_eq!(report.coverage.unrecognized, 0);
        let h0 = &report.hours.iter().find(|h| h.hour == hour_n(0)).unwrap();
        assert_eq!(h0.refusals, 2);
        // The unavailable reading never invents a zero in the hour's median.
        let h0_load = h0.load_15m_median.unwrap_or(0.0);
        assert!(
            (h0_load - 98.15).abs() < 0.01,
            "median of 75.9 and 120.4, got {h0_load}"
        );
        assert_eq!(h0.live_workers_last, Some(7.0));
        assert_eq!(h0.compressor_gb_median, Some(2.5));
        assert_eq!(h0.swap_gb_median, Some(9.1));
        let now_r = report.now.reading.expect("newest reading");
        assert_eq!(now_r.verdict, "calm");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ac7_reworded_detail_counts_unrecognized_never_zero() {
        let dir = tmp_dir("ac7");
        let home_dir = dir.join("fno").join("agents");
        write_lines(
            &home_dir.join("events.jsonl"),
            &format!(
                "{{\"ts\":\"{}\",\"type\":\"control_plane_tick\",\"source\":\"loop\",\"data\":{{\"arm\":\"machine_watch\",\"scheduler\":\"daemon\",\"acted\":1,\"interval_s\":300,\"skip_reason\":null,\"detail\":\"machine heavy above band 90%; load_15m high, plenty runnable of many processes\"}}}}\n",
                stamp(0, 5)
            ),
        );
        let home = pinned_home(&dir);
        let report = analyze(&inputs(&dir, home, None));
        assert_eq!(report.coverage.unrecognized, 1);
        assert!(
            report.hours.iter().all(|h| h.load_15m_median.is_none()),
            "a reworded sentence never plots a zero"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ac8_empty_state_prints_every_reason() {
        let dir = tmp_dir("ac8");
        let home = pinned_home(&dir);
        let report = analyze(&inputs(&dir, home, None));
        let text = render_text(&report);
        assert!(text.contains("now: no machine_watch reading yet"));
        assert!(text.contains("no capacity curve: no machine_watch readings in the window"));
        assert!(text.contains("no threshold: 0 slowdown turns with a reading, 2 needed"));
        assert!(text.contains("no suggested cap: 0 slowdown turns with a reading, 2 needed"));
        assert!(text.contains("no slowdown turns in the window"));
        assert_eq!(
            run_fleet_cli(&["--fleet".into(), "--days".into(), "x".into()]),
            2
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ac9_bare_intel_dispatch_is_untouched() {
        // The fleet dispatch never fires without --fleet, and the flag parse
        // exits stay pure (no env, no state access before a bad --days).
        assert_eq!(
            run_fleet_cli(&["--fleet".into(), "--days".into(), "x".into()]),
            2
        );
        assert_eq!(
            run_fleet_cli(&["--fleet".into(), "--days".into(), "0".into()]),
            2,
            "--days 0 has no consistent fleet meaning; it is rejected"
        );
        assert_eq!(run_fleet_cli(&["--nonsense".into()]), 2);
        assert_eq!(run_fleet_cli(&["--help".into()]), 0);
    }

    #[test]
    fn ac17_pending_backfill_withholds_curve_threshold_cap() {
        let dir = build_curve_fixture("ac17");
        let home = pinned_home(&dir);
        // A tiny budget leaves every transcript unread.
        let report = analyze(&inputs(&dir, home, Some(50)));
        assert!(report.curve.is_none());
        assert_eq!(report.threshold, None);
        assert_eq!(report.cap, None);
        assert!(report.curve_reason.contains("backfill incomplete"));
        assert!(report.curve_reason.contains("--backfill"));
        assert!(
            report.coverage.fold.pending_files >= 1,
            "the receipt names the pending files"
        );
        let text = render_text(&report);
        assert!(text.contains("backfill incomplete"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
