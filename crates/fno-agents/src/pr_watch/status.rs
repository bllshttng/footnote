//! The `pr-watch status` verb: the one liveness leg.
//!
//! Port of the Python `liveness_report` / `liveness_report_live` pair in
//! `cli/src/fno/pr_watch/_install.py`, retired in the same change that
//! landed this module (the port protocol,
//! docs/architecture/dual-implementation-inventory.md). The parity test
//! `tests/pr_watch_status_parity.rs` freezes the JSON verdict keys, the
//! `dead` and `wedged` verdict words, and the human readout against goldens
//! captured from the Python leg. The tick-state facts ride the arms fold
//! (`tick_ledger::read_arms` + `explain`) - the leg the 2026-09-15 verdict
//! split taught the fleet to trust - while the install-state facts the arms
//! table cannot see (config enabled, plist, launchctl load state) stay
//! local, exactly as they were in the Python leg.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

pub(crate) use super::render::{build_json, render_text};

/// `timeout` and `error` are the outcomes that mean the tick broke;
/// lock_held, quota_skip and disabled are benign.
const BROKEN_OUTCOMES: &[&str] = &["timeout", "error"];

/// How many tail end records the watermark pass keeps for the wedged streak
/// (oldest first). Caps the streak any config knob can see.
const RECENT_ENDS_KEEP: usize = 16;

/// The LaunchAgent label and its plist file name, as the installer writes it.
const LABEL: &str = "sh.fno.pr-watcher";
pub(crate) const PLIST_FILENAME: &str = "sh.fno.pr-watcher.plist";

/// The `pr_watch` config slice the verdict reads.
#[derive(Debug, Clone)]
pub(crate) struct Cfg {
    pub enabled: bool,
    pub interval_seconds: i64,
    pub wedged_after_ticks: i64,
}

impl Default for Cfg {
    fn default() -> Self {
        Cfg {
            enabled: false,
            interval_seconds: 600,
            wedged_after_ticks: 3,
        }
    }
}

/// The config carrier is the crate's own: first candidate holding the dotted
/// key wins, the same precedence the Python loader merges.
pub(crate) fn cfg_lookup(cwd: &Path, path: &[&str]) -> Option<toml::Value> {
    crate::agents_config::config_lookup(cwd, path)
}

/// Pydantic-lax bool: real booleans, "true"/"false" strings, 1/0.
pub(crate) fn as_bool(value: Option<toml::Value>, default: bool) -> bool {
    match value {
        Some(toml::Value::Boolean(b)) => b,
        Some(toml::Value::String(s)) => match s.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" | "y" | "t" => true,
            "false" | "0" | "no" | "off" | "n" | "f" => false,
            _ => default,
        },
        Some(toml::Value::Integer(i)) if i == 1 => true,
        Some(toml::Value::Integer(i)) if i == 0 => false,
        _ => default,
    }
}

fn as_i64(value: Option<toml::Value>, default: i64) -> i64 {
    match value {
        Some(toml::Value::Integer(i)) if i > 0 => i,
        _ => default,
    }
}

fn load_cfg(cwd: &Path) -> Cfg {
    Cfg {
        enabled: as_bool(cfg_lookup(cwd, &["pr_watch", "enabled"]), false),
        interval_seconds: as_i64(cfg_lookup(cwd, &["pr_watch", "interval_seconds"]), 600),
        wedged_after_ticks: as_i64(cfg_lookup(cwd, &["pr_watch", "wedged_after_ticks"]), 3),
    }
}

/// One tick end record, shaped exactly as the watermark fold kept it.
#[derive(Debug, Clone, Default)]
pub(crate) struct EndRec {
    pub ts: Option<String>,
    pub outcome: Option<String>,
    pub phase: Option<String>,
    pub duration_s: Option<f64>,
    pub sweep_failures: Option<i64>,
    pub saturated: Option<Vec<String>>,
}

impl EndRec {
    fn from_value(ts: Option<&str>, data: &Value) -> EndRec {
        EndRec {
            ts: ts.map(str::to_string),
            outcome: str_field(data, "outcome"),
            phase: str_field(data, "phase"),
            duration_s: data.get("duration_s").and_then(Value::as_f64),
            sweep_failures: data.get("sweep_failures").and_then(Value::as_i64),
            saturated: data.get("saturated").and_then(Value::as_array).map(|a| {
                a.iter()
                    .map(|v| match v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .collect()
            }),
        }
    }

    fn broken(&self) -> bool {
        self.outcome
            .as_deref()
            .is_some_and(|o| BROKEN_OUTCOMES.contains(&o))
    }
}

fn str_field(data: &Value, key: &str) -> Option<String> {
    data.get(key).and_then(Value::as_str).map(str::to_string)
}

/// The merge scan receipt the newest tick carried, if any.
#[derive(Debug, Clone)]
pub(crate) struct MergeScan {
    pub completed: bool,
    pub completed_at: String,
    pub scanned: Value,
}

/// One `control_plane_tick` arm row, as the cut check reads it.
#[derive(Debug, Clone)]
pub(crate) struct ArmRow {
    pub ts: Option<String>,
    pub skip_reason: Option<String>,
    pub detail: String,
}

/// The single-pass watermark fold over the events journal.
#[derive(Debug, Default)]
pub(crate) struct Marks {
    pub last_tick: Option<String>,
    pub last_attempt: Option<String>,
    pub last_end: Option<EndRec>,
    pub completed_tick: Option<(String, i64)>,
    pub recent_ends: Vec<EndRec>,
    pub merge_scan: Option<MergeScan>,
    pub arm_rows: Vec<(String, ArmRow)>,
}

pub(crate) fn parse_ts(ts: Option<&str>) -> Option<f64> {
    crate::tick_ledger::parse_rfc3339_unix(ts?).map(|unix| unix as f64)
}

/// Fold every watermark from one pass over the journal's committed rows plus
/// its live bytes.
pub(crate) fn tick_watermarks(events: &Path) -> Marks {
    let mut marks = Marks::default();
    let mut chunks_by_receipt: HashMap<String, Vec<Value>> = HashMap::new();
    let types = [
        "pr_watch_tick",
        "pr_watch_tick_attempt",
        "pr_watch_tick_end",
        "pr_watch_sweep_chunk",
        "control_plane_tick",
    ];
    let text = crate::event_store::journal_text(events, &types);
    for line in text.lines() {
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(etype) = row.get("type").and_then(Value::as_str) else {
            continue;
        };
        let ts = row.get("ts").and_then(Value::as_str);
        let data = row.get("data").cloned().unwrap_or(Value::Null);
        match etype {
            "pr_watch_sweep_chunk" => {
                if let Some(id) = data.get("receipt_id").and_then(Value::as_str) {
                    chunks_by_receipt
                        .entry(id.to_string())
                        .or_default()
                        .push(data);
                }
            }
            "pr_watch_tick" => {
                marks.last_tick = ts.map(str::to_string);
                if let Some(scan) = data.get("merge_scan").filter(|s| s.is_object()) {
                    marks.merge_scan = Some(MergeScan {
                        completed: scan.get("completed").and_then(Value::as_bool) == Some(true),
                        completed_at: ts.unwrap_or_default().to_string(),
                        scanned: scan.get("scanned").cloned().unwrap_or(Value::Null),
                    });
                }
                if let Some(done) = valid_completed_tick(ts, &data, &chunks_by_receipt) {
                    marks.completed_tick = Some(done);
                }
            }
            "pr_watch_tick_attempt" => {
                marks.last_attempt = ts.map(str::to_string);
            }
            "pr_watch_tick_end" => {
                let end = EndRec::from_value(ts, &data);
                marks.recent_ends.push(end.clone());
                if marks.recent_ends.len() > RECENT_ENDS_KEEP {
                    let overflow = marks.recent_ends.len() - RECENT_ENDS_KEEP;
                    marks.recent_ends.drain(..overflow);
                }
                marks.last_end = Some(end);
            }
            "control_plane_tick" => {
                if let Some(arm) = data.get("arm").and_then(Value::as_str) {
                    let detail = match data.get("detail") {
                        Some(Value::String(s)) => s.chars().take(200).collect(),
                        Some(other) if !other.is_null() => {
                            other.to_string().chars().take(200).collect()
                        }
                        _ => String::new(),
                    };
                    let row = ArmRow {
                        ts: ts.map(str::to_string),
                        skip_reason: str_field(&data, "skip_reason"),
                        detail,
                    };
                    match marks.arm_rows.iter_mut().find(|(name, _)| name == arm) {
                        Some(slot) => slot.1 = row,
                        None => marks.arm_rows.push((arm.to_string(), row)),
                    }
                }
            }
            _ => {}
        }
    }
    marks
}

/// The positive completion marker, only for a coherent sweep receipt. The
/// chunked form rebuilds `swept` from `pr_watch_sweep_chunk` rows and rejects
/// any incoherence: a duplicate identity, a bad key, a chunk count mismatch.
fn valid_completed_tick(
    ts: Option<&str>,
    data: &Value,
    chunks_by_receipt: &HashMap<String, Vec<Value>>,
) -> Option<(String, i64)> {
    let ts = ts?;
    if parse_ts(Some(ts)).is_none() || !data.is_object() {
        return None;
    }
    let swept_count = match data.get("swept_count") {
        Some(Value::Number(n)) => n.as_i64()?,
        _ => return None,
    };
    if swept_count <= 0 {
        return None;
    }
    let Some(Value::Object(swept_map)) = data.get("swept") else {
        return None;
    };
    let mut swept: Vec<(String, Vec<i64>)> = if swept_map.is_empty() {
        let receipt_id = match data.get("receipt_id") {
            Some(Value::String(s)) => s.clone(),
            _ => return None,
        };
        let expected = match data.get("receipt_chunks") {
            Some(Value::Number(n)) => n.as_i64()?,
            _ => return None,
        };
        if expected <= 0 {
            return None;
        }
        let chunks = chunks_by_receipt.get(&receipt_id)?;
        if chunks.is_empty() || chunks.len() as i64 != expected {
            return None;
        }
        let mut sorted: Vec<&Value> = chunks.iter().collect();
        sorted.sort_by_key(|chunk| {
            chunk
                .get("chunk_index")
                .and_then(Value::as_i64)
                .unwrap_or(0)
        });
        let mut rebuilt: Vec<(String, Vec<i64>)> = Vec::new();
        let mut seen: Vec<(String, i64)> = Vec::new();
        for chunk in sorted {
            let Some(Value::Array(items)) = chunk.get("items") else {
                return None;
            };
            for item in items {
                if item.get("action").and_then(Value::as_str) != Some("swept") {
                    continue;
                }
                let Some(Value::String(key)) = item.get("key") else {
                    return None;
                };
                let Some((repo, number_text)) = key.rsplit_once('#') else {
                    return None;
                };
                if repo.is_empty()
                    || number_text.is_empty()
                    || !number_text.bytes().all(|b| b.is_ascii_digit())
                {
                    return None;
                }
                let number: i64 = number_text.parse().ok()?;
                if number <= 0 {
                    return None;
                }
                if seen.contains(&(repo.to_string(), number)) {
                    return None;
                }
                seen.push((repo.to_string(), number));
                match rebuilt.iter_mut().find(|(name, _)| name == repo) {
                    Some((_, numbers)) => numbers.push(number),
                    None => rebuilt.push((repo.to_string(), vec![number])),
                }
            }
        }
        rebuilt
    } else {
        let mut direct: Vec<(String, Vec<i64>)> = Vec::new();
        for (repo, numbers) in swept_map {
            let Value::Array(list) = numbers else {
                return None;
            };
            let mut parsed: Vec<i64> = Vec::new();
            for number in list {
                let Value::Number(n) = number else {
                    return None;
                };
                parsed.push(n.as_i64()?);
            }
            direct.push((repo.clone(), parsed));
        }
        direct
    };

    let mut identities: Vec<(String, i64)> = Vec::new();
    for (repo, numbers) in swept.drain(..) {
        if repo.is_empty() {
            return None;
        }
        for number in numbers {
            if number <= 0 {
                return None;
            }
            identities.push((repo.clone(), number));
        }
    }
    if identities.len() != swept_count as usize {
        return None;
    }
    let mut unique = identities.clone();
    unique.sort();
    unique.dedup();
    if unique.len() != swept_count as usize {
        return None;
    }
    Some((ts.to_string(), swept_count))
}

/// The parenthesised detail bits after a tick outcome: duration, sweep
/// failures, the phases that spent their whole slice, and the phase name only
/// when the tick broke.
pub(crate) fn tick_end_bits(end: &EndRec) -> Vec<String> {
    let mut bits: Vec<String> = Vec::new();
    if let Some(duration) = end.duration_s {
        bits.push(format!("{duration:.1}s"));
    }
    if end.sweep_failures.is_some_and(|f| f != 0) {
        bits.push(format!("{} sweep failures", end.sweep_failures.unwrap()));
    }
    if let Some(saturated) = &end.saturated {
        if !saturated.is_empty() {
            bits.push(format!("saturated: {}", saturated.join(", ")));
        }
    }
    if end.phase.is_some() && end.broken() {
        bits.push(format!("phase: {}", end.phase.clone().unwrap_or_default()));
    }
    bits
}

/// Consecutive broken tick ends at the tail of an oldest-first list.
pub(crate) fn broken_streak(ends: &[EndRec]) -> usize {
    ends.iter().rev().take_while(|end| end.broken()).count()
}

/// `fno do pr watch status` (and later the other five verbs) dispatches here
/// from `bin/client.rs` beside `pr-park`.
pub fn run(args: &[String]) -> i32 {
    let mut json_out = false;
    for arg in args {
        match arg.as_str() {
            "--json" | "-J" => json_out = true,
            other => {
                eprintln!("fno-agents pr-watch status: unknown argument {other}");
                return 2;
            }
        }
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let launch_agents_dir = std::env::var_os("FNO_PR_WATCH_TEST_LAUNCH_AGENTS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(|home| Path::new(&home).join("Library").join("LaunchAgents"))
                .unwrap_or_else(|| PathBuf::from("Library").join("LaunchAgents"))
        });
    let inp = gather(&cwd, &launch_agents_dir);
    let marks = tick_watermarks(&inp.events);
    let leg = leg_facts(&inp);
    let verdict = verdict(&inp, &marks, &leg);
    if json_out {
        println!("{}", build_json(&verdict, &marks, &inp));
    } else {
        let (out, err) = render_text(&inp, &marks, &verdict);
        print!("{out}");
        eprint!("{err}");
    }
    0
}

/// Everything the verdict and the readout need from the world, injectable so
/// the parity test can pin every input.
#[derive(Debug, Clone)]
pub(crate) struct Inputs {
    pub cfg: Cfg,
    pub loaded: bool,
    pub launch_agents_dir: PathBuf,
    pub plist_mtime: Option<f64>,
    pub now: f64,
    pub events: PathBuf,
    pub watcher_state: PathBuf,
    pub cwd: PathBuf,
}

/// The state root every status file hangs off, resolved the way `pr-park`
/// resolves its own paths: the configured `state_dir` key, else the fno home.
pub(crate) fn state_root(cwd: &Path) -> PathBuf {
    crate::pr_park::Paths::resolve(cwd)
        .state
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".fno"))
}

pub(crate) fn gather(cwd: &Path, launch_agents_dir: &Path) -> Inputs {
    let cfg = load_cfg(cwd);
    let loaded = launchctl_is_loaded();
    let plist_mtime = std::fs::metadata(launch_agents_dir.join(PLIST_FILENAME))
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let park_paths = crate::pr_park::Paths::resolve(cwd);
    Inputs {
        cfg,
        loaded,
        launch_agents_dir: launch_agents_dir.to_path_buf(),
        plist_mtime,
        now,
        events: park_paths.events.clone(),
        watcher_state: park_paths.state.clone(),
        cwd: cwd.to_path_buf(),
    }
}

/// True when the watcher's label appears in `launchctl list` output. The
/// env pin is test-only: the parity fixtures inject the load state the way
/// the Python tests monkeypatch `_launchctl_is_loaded`, so the goldens do
/// not depend on the capture machine's own registration.
pub(crate) fn launchctl_is_loaded() -> bool {
    if let Ok(pin) = std::env::var("FNO_PR_WATCH_TEST_LOADED") {
        return pin == "1";
    }
    std::process::Command::new("launchctl")
        .arg("list")
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).contains(LABEL))
        .unwrap_or(false)
}

/// The tick-state facts the surviving arms leg holds: staleness and failure
/// on the tick's own two rows, judged by `tick_ledger::explain` over the
/// journal fold. `DaemonFacts::Unknown`: the status read carries no daemon
/// probe, and every daemon-conditional rule keeps the row untouched.
pub(crate) struct LegFacts {
    pub stale: bool,
    pub failing: bool,
}

pub(crate) fn leg_facts(inp: &Inputs) -> LegFacts {
    let journals = vec![inp.events.clone()];
    let mut rows = crate::tick_ledger::read_arms(&journals, inp.now as u64);
    let trace = crate::tick_ledger::read_tick_trace(&journals, inp.now as u64);
    crate::tick_ledger::explain_with_trace(
        &mut rows,
        &crate::tick_ledger::DaemonFacts::Unknown,
        &trace,
    );
    let is_watch = |arm: &str| arm == "pr_watch_merge" || arm == "pr_watch_sweep";
    // A cut (timeout / starved) routes to the unhealthy branch below; it must
    // not also read as a failure here, or a fresh watermark with one cut row
    // renders `wedged` over a streak of zero broken ticks.
    let is_cut = |r: &crate::tick_ledger::ArmStatus| {
        matches!(r.skip_reason.as_deref(), Some("timeout") | Some("starved"))
    };
    LegFacts {
        stale: rows.iter().any(|r| is_watch(&r.arm) && r.stale),
        failing: rows
            .iter()
            .any(|r| is_watch(&r.arm) && r.failing && !is_cut(r)),
    }
}

/// The verdict the JSON reports, one of `disabled | dead | healthy |
/// healthy-pending | wedged | unhealthy`. The rule tree is the Python leg's,
/// byte for byte; the tick facts ride the arms fold. A freshly installed
/// agent with no tick yet reads `healthy-pending`, not `dead`; a bounce over
/// a broken streak reads `wedged` - a bounce is not evidence of a cure.
pub(crate) struct Verdict {
    pub enabled: bool,
    pub word: String,
    pub detail: String,
    pub fix: Option<String>,
    pub bounce_pending: bool,
}

pub(crate) fn verdict(inp: &Inputs, marks: &Marks, leg: &LegFacts) -> Verdict {
    let interval = inp.cfg.interval_seconds;
    let threshold = 2 * interval.max(1);
    let base = Verdict {
        enabled: inp.cfg.enabled,
        word: String::new(),
        detail: String::new(),
        fix: None,
        bounce_pending: false,
    };
    if !inp.cfg.enabled {
        return Verdict {
            word: "disabled".into(),
            detail: "pr_watch.enabled=false".into(),
            ..base
        };
    }
    let plist_exists = inp.plist_mtime.is_some();
    if !plist_exists {
        return Verdict {
            word: "dead".into(),
            detail: "enabled but no LaunchAgent plist installed".into(),
            fix: Some("fno do pr watch install".into()),
            ..base
        };
    }
    if !inp.loaded {
        return Verdict {
            word: "dead".into(),
            detail: "plist present but agent not loaded".into(),
            fix: Some("fno do pr watch install".into()),
            ..base
        };
    }
    let plist_mtime = inp.plist_mtime.unwrap_or_default();

    // The post-install grace must not cover a tick that already ran and
    // broke: an end newer than the plist IS the first post-install tick's
    // outcome.
    let broke = marks.last_end.as_ref().filter(|end| {
        end.broken()
            && parse_ts(end.ts.as_deref())
                .map(|end_epoch| end_epoch > plist_mtime)
                .unwrap_or(false)
    });
    let broke_suffix = |end: &EndRec| {
        format!(
            "post-install tick ended {} ({}) without completing",
            end.outcome.clone().unwrap_or_default(),
            tick_end_bits(end).join(", ")
        )
    };

    let tick_epoch = parse_ts(marks.last_tick.as_deref());
    let bounce_pending = broke.is_none()
        && (inp.now - plist_mtime) < threshold as f64
        && tick_epoch.map(|tick| plist_mtime > tick).unwrap_or(true);
    if bounce_pending {
        let streak = broken_streak(&marks.recent_ends);
        if streak >= inp.cfg.wedged_after_ticks.max(1) as usize {
            let last = match tick_epoch {
                Some(tick) => format!("last completed tick {}s ago", (inp.now - tick) as i64),
                None => "no completed tick recorded".to_string(),
            };
            return Verdict {
                word: "wedged".into(),
                detail: format!(
                    "bounced {}s ago over {} consecutive broken ticks; {last}; \
                     no tick has ended since the bounce, so it is not yet a cure",
                    (inp.now - plist_mtime) as i64,
                    streak
                ),
                fix: Some("fno agents status".into()),
                bounce_pending: true,
                ..base
            };
        }
        return Verdict {
            word: "healthy-pending".into(),
            detail: "installed recently; awaiting first tick".into(),
            bounce_pending: true,
            ..base
        };
    }
    let Some(tick_epoch) = tick_epoch else {
        if let Some(end) = broke {
            return Verdict {
                word: "dead".into(),
                detail: format!(
                    "installed {}s ago; {}",
                    (inp.now - plist_mtime) as i64,
                    broke_suffix(end)
                ),
                fix: Some("fno agents status".into()),
                ..base
            };
        }
        return Verdict {
            word: "dead".into(),
            detail: format!(
                "no tick recorded and installed more than 2x interval ({threshold}s) ago"
            ),
            fix: Some("fno do pr watch install".into()),
            ..base
        };
    };

    let age = inp.now - tick_epoch;
    if age > threshold as f64 || leg.stale {
        let detail = if let Some(end) = broke {
            format!(
                "last tick {}s ago (> 2x interval {threshold}s); {}",
                age as i64,
                broke_suffix(end)
            )
        } else {
            format!("last tick {}s ago (> 2x interval {threshold}s)", age as i64)
        };
        let fix = if broke.is_some() {
            "fno agents status"
        } else {
            "fno do pr watch install"
        };
        return Verdict {
            word: "dead".into(),
            detail,
            fix: Some(fix.into()),
            ..base
        };
    }
    let streak = broken_streak(&marks.recent_ends);
    if streak >= inp.cfg.wedged_after_ticks.max(1) as usize || leg.failing {
        return Verdict {
            word: "wedged".into(),
            detail: format!(
                "last tick {}s ago but each of the last {streak} ticks ended broken; \
                 the watermark is fresh, so the watcher is up and delivering nothing",
                age as i64
            ),
            fix: Some("fno do pr watch refresh".into()),
            ..base
        };
    }

    // An arm row the tick carried that cut (timeout or starved) inside twice
    // the interval makes the watcher read unhealthy: up is not delivering.
    let window = threshold as f64;
    let cut_names: Vec<String> = marks
        .arm_rows
        .iter()
        .filter(|(_, row)| {
            matches!(
                row.skip_reason.as_deref(),
                Some("timeout") | Some("starved")
            )
        })
        .filter(|(_, row)| {
            parse_ts(row.ts.as_deref())
                .map(|ts| inp.now - ts)
                .map(|cut_age| (0.0..=window).contains(&cut_age))
                .unwrap_or(false)
        })
        .map(|(arm, row)| {
            let detail = if row.detail.is_empty() {
                "none"
            } else {
                row.detail.as_str()
            };
            format!("{arm} cut: {detail}")
        })
        .collect();
    if !cut_names.is_empty() {
        let named: String = cut_names.join("; ").chars().take(400).collect();
        return Verdict {
            word: "unhealthy".into(),
            detail: named,
            fix: Some("fno agents status".into()),
            ..base
        };
    }
    Verdict {
        word: "healthy".into(),
        detail: format!("last tick {}s ago", age as i64),
        ..base
    }
}
