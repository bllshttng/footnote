//! `evals-trend`: the eval-history report fold and the time-window
//! trend, native under d-b6cc1a2a (all new code lands in crates; Python is a
//! thin caller). One transport-only action behind two Python leaves:
//! `fno doctor evals report` forwards `--mode report`, `fno doctor evals
//! trend` forwards `--mode trend`, and `evals_health_summary` reads the
//! windowed alarm and `regressed` through `--mode summary`.
//!
//! Semantics are the Python fold they replace (`cli/src/fno/evals/report.py`
//! at this PR's base): tier-segment stats, half-open windows, modal-rev
//! variant compare, graduation candidates.
use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::DateTime;
use chrono::Utc;
use serde_json::json;
use serde_json::Map;
use serde_json::Value;

const EXIT_USAGE: i32 = 2;
const USAGE: &str = "usage: fno-agents evals-trend --history <jsonl> (--mode report [--since K] [--graduate N] [--json] [--compare V] [--planned <json>] | --mode trend | --mode summary) [--stale-days N] [--now <rfc3339>]";

/// One history row: the fields the folds read. Absent keys read as the
/// Python fold read them (missing `variant` is baseline, missing `ts` is
/// unparseable, missing `tier` is "unknown"). `raw` is kept so the attempt
/// verdict comes from the ONE native classifier, never a second fold.
#[derive(Clone)]
struct Row {
    task_id: String,
    tier: String,
    pass: bool,
    ts: Option<DateTime<Utc>>,
    variant: Option<String>,
    bank_rev: Option<String>,
    raw: Value,
}

fn read_rows(history: &str, variant: Option<&str>, since: Option<usize>) -> Vec<Row> {
    let text = std::fs::read_to_string(history).unwrap_or_default();
    let mut rows: Vec<Row> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let row_variant = v
            .get("variant")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| "baseline".into());
        if let Some(want) = variant {
            if row_variant != want {
                continue;
            }
        }
        rows.push(Row {
            task_id: v
                .get("task_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            tier: v
                .get("tier")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
            pass: v.get("pass") == Some(&Value::Bool(true)),
            ts: v
                .get("ts")
                .and_then(Value::as_str)
                .and_then(|ts| DateTime::parse_from_rfc3339(ts).ok())
                .map(|dt| dt.with_timezone(&Utc)),
            variant: v.get("variant").and_then(Value::as_str).map(str::to_string),
            bank_rev: v
                .get("bank_rev")
                .and_then(Value::as_str)
                .map(str::to_string),
            raw: v,
        });
    }
    if let Some(k) = since {
        let at = rows.len().saturating_sub(k);
        rows.drain(..at);
    }
    rows
}

/// Per-task stats over one row list: attempts/passes since the task's latest
/// tier change (the anti-false-alarm segment rule).
///
/// Two denominators: a task with ANY natively classified row reports
/// `grades` (valid task grades) and correctness over those grades only -
/// infrastructure/unavailable/ungraded attempts never drag the pass rate. A
/// task whose rows are ALL legacy (pre-attempt history) keeps the old
/// boolean fold: legacy evidence is never reinterpreted, and existing
/// history keeps its alarm semantics. `runs` is the attempt count either way.
struct TaskStat {
    task_id: String,
    tier: String,
    runs: usize,
    passes: usize,
    grades: usize,
    infrastructure: usize,
    unavailable: usize,
    ungraded: usize,
    legacy: usize,
    legacy_fold: bool,
}

impl TaskStat {
    /// Correctness denominator: valid grades for a modern task, attempts for
    /// an all-legacy task.
    fn grade_denominator(&self) -> usize {
        if self.legacy_fold {
            self.runs
        } else {
            self.grades
        }
    }
    fn pass_at_1(&self) -> f64 {
        let den = self.grade_denominator();
        if den == 0 {
            0.0
        } else {
            self.passes as f64 / den as f64
        }
    }
    fn pass_k(&self) -> bool {
        let den = self.grade_denominator();
        den > 0 && self.passes == den
    }
    fn flake(&self) -> bool {
        let den = self.grade_denominator();
        self.passes > 0 && self.passes < den
    }
}

fn stats(rows: &[Row]) -> Vec<TaskStat> {
    let mut by_task: BTreeMap<String, Vec<&Row>> = BTreeMap::new();
    for r in rows {
        by_task.entry(r.task_id.clone()).or_default().push(r);
    }
    let mut out = Vec::new();
    for (tid, task_rows) in by_task {
        let current_tier = task_rows
            .last()
            .map(|r| r.tier.clone())
            .unwrap_or_else(|| "unknown".into());
        let mut segment_rows: Vec<&Row> = Vec::new();
        for r in task_rows.iter().rev() {
            if r.tier != current_tier {
                break;
            }
            segment_rows.push(r);
        }
        let mut stat = TaskStat {
            task_id: tid,
            tier: current_tier,
            runs: segment_rows.len(),
            passes: 0,
            grades: 0,
            infrastructure: 0,
            unavailable: 0,
            ungraded: 0,
            legacy: 0,
            legacy_fold: false,
        };
        for r in &segment_rows {
            match crate::eval_attempt::classify(&r.raw, None).status {
                "graded" => {
                    stat.grades += 1;
                    if r.pass {
                        stat.passes += 1;
                    }
                }
                "infrastructure" => stat.infrastructure += 1,
                "unavailable" => stat.unavailable += 1,
                "ungraded" => stat.ungraded += 1,
                _ => stat.legacy += 1,
            }
        }
        // All-legacy segment: keep the pre-attempt boolean fold verbatim.
        stat.legacy_fold =
            stat.grades + stat.infrastructure + stat.unavailable + stat.ungraded == 0;
        if stat.legacy_fold {
            stat.passes = segment_rows.iter().filter(|r| r.pass).count();
        }
        out.push(stat);
    }
    out
}

/// `stats` over the rows whose ts lands in `(start, end]`.
fn stats_window(rows: &[Row], start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<TaskStat> {
    let kept: Vec<Row> = rows
        .iter()
        .filter(|r| match r.ts {
            Some(dt) => start < dt && dt <= end,
            None => false,
        })
        .cloned()
        .collect();
    stats(&kept)
}

fn round4(x: f64) -> f64 {
    (x * 10000.0).round() / 10000.0
}

fn window_rows<'a>(rows: &'a [Row], start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<&'a Row> {
    rows.iter()
        .filter(|r| match r.ts {
            Some(dt) => start < dt && dt <= end,
            None => false,
        })
        .collect()
}

/// Per-task verdict of one row list against another: runs, pass@1 per side,
/// delta (b - a) and the improved/regressed/unchanged verdict.
fn pair_verdict(a: &[&Row], b: &[&Row]) -> (usize, f64, usize, f64, f64, &'static str) {
    let a_p1 = a.iter().filter(|r| r.pass).count() as f64 / a.len() as f64;
    let b_p1 = b.iter().filter(|r| r.pass).count() as f64 / b.len() as f64;
    let delta = b_p1 - a_p1;
    let verdict = if delta > 0.0 {
        "improved"
    } else if delta < 0.0 {
        "regressed"
    } else {
        "unchanged"
    };
    (a.len(), a_p1, b.len(), b_p1, delta, verdict)
}

/// The trend fold: recent `(now - W, now]` against prior `(now - 2W, now - W]`,
/// plus `regressed` (regression-tier tasks whose verdict is regressed) and the
/// recent-window regression alarm.
fn trend_fold(rows: &[Row], window_days: i64, now: DateTime<Utc>) -> (Value, Vec<String>) {
    let w = chrono::Duration::days(window_days);
    let prior: BTreeMap<String, Vec<&Row>> = group_by_task(window_rows(rows, now - w - w, now - w));
    let recent: BTreeMap<String, Vec<&Row>> = group_by_task(window_rows(rows, now - w, now));
    let mut tasks = Map::new();
    let mut missing_in_prior = Vec::new();
    let mut missing_in_recent = Vec::new();
    let mut ids: Vec<&String> = prior.keys().chain(recent.keys()).collect();
    ids.sort();
    ids.dedup();
    for tid in ids {
        let p = prior.get(tid).cloned().unwrap_or_default();
        let r = recent.get(tid).cloned().unwrap_or_default();
        if p.is_empty() {
            missing_in_prior.push(tid.clone());
        }
        if r.is_empty() {
            missing_in_recent.push(tid.clone());
        }
        if p.is_empty() || r.is_empty() {
            continue;
        }
        let (pr, pp1, rr, rp1, delta, verdict) = pair_verdict(&p, &r);
        tasks.insert(
            tid.clone(),
            json!({
                "prior": {"runs": pr, "pass_at_1": round4(pp1)},
                "recent": {"runs": rr, "pass_at_1": round4(rp1)},
                "delta": round4(delta),
                "verdict": verdict,
            }),
        );
    }
    let newest_tier: BTreeMap<String, String> = group_by_task(rows.iter().collect())
        .into_iter()
        .map(|(tid, trs)| {
            let tier = trs
                .last()
                .map(|r| r.tier.clone())
                .unwrap_or_else(|| "unknown".into());
            (tid, tier)
        })
        .collect();
    let regressed: Vec<String> = tasks
        .iter()
        .filter(|(tid, t)| {
            t["verdict"] == "regressed"
                && newest_tier.get(*tid).map(String::as_str) == Some("regression")
        })
        .map(|(tid, _)| tid.clone())
        .collect();
    let recent_alarm: Vec<String> = stats_window(rows, now - w, now)
        .into_iter()
        .filter(|s| s.tier == "regression" && s.pass_at_1() < 1.0)
        .map(|s| s.task_id)
        .collect();
    let view = json!({
        "window_days": window_days,
        "prior_start": (now - w - w).to_rfc3339(),
        "recent_start": (now - w).to_rfc3339(),
        "tasks": tasks,
        "missing_in_prior": missing_in_prior,
        "missing_in_recent": missing_in_recent,
        "regressed": regressed,
        "regression_alarm": recent_alarm,
    });
    (view, regressed)
}

fn group_by_task<'a>(rows: Vec<&'a Row>) -> BTreeMap<String, Vec<&'a Row>> {
    let mut out: BTreeMap<String, Vec<&Row>> = BTreeMap::new();
    for r in rows {
        out.entry(r.task_id.clone()).or_default().push(r);
    }
    out
}

/// The report fold: tiers, tasks, flakes, the windowed alarm, graduation.
/// *planned* (task_id -> expected attempts) projects the planned denominator:
/// missing attempts stay visible and completion is reported beside correctness.
fn report_fold(
    rows: &[Row],
    window_days: i64,
    now: DateTime<Utc>,
    graduate_n: Option<usize>,
    planned: Option<&BTreeMap<String, usize>>,
) -> Value {
    let all = stats(rows);
    let mut tiers: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for s in &all {
        let e = tiers.entry(s.tier.clone()).or_insert((0, 0));
        e.0 += s.runs;
        e.1 += s.passes;
        // The tier rate sums each task's OWN denominator (grades for a modern
        // task, attempts for an all-legacy task) so the aggregate never mixes
        // infrastructure evidence into correctness.
    }
    let tier_json = Map::from_iter(tiers.iter().map(|(tier, (runs, passes))| {
        let den: usize = all
            .iter()
            .filter(|s| s.tier == *tier)
            .map(|s| s.grade_denominator())
            .sum();
        let rate = if den == 0 {
            0.0
        } else {
            *passes as f64 / den as f64
        };
        (
            tier.clone(),
            json!({"runs": runs, "passes": passes, "pass_rate": round4(rate)}),
        )
    }));
    let tasks: Vec<Value> = all
        .iter()
        .map(|s| {
            let mut task = json!({
                "task_id": s.task_id, "tier": s.tier, "runs": s.runs,
                "passes": s.passes, "pass_at_1": round4(s.pass_at_1()),
                "pass_k": s.pass_k(), "flake": s.flake(),
                "grades": s.grades,
                "grade_den": s.grade_denominator(),
                "attempts": {
                    "infrastructure": s.infrastructure,
                    "unavailable": s.unavailable,
                    "ungraded": s.ungraded,
                    "legacy": s.legacy,
                },
                "legacy_fold": s.legacy_fold,
            });
            if let Some(plan) = planned {
                let expected = plan.get(&s.task_id).copied().unwrap_or(0);
                let missing = expected.saturating_sub(s.runs);
                let completion = if expected == 0 {
                    0.0
                } else {
                    s.runs as f64 / expected as f64
                };
                task["expected_attempts"] = json!(expected);
                task["missing_attempts"] = json!(missing);
                task["completion"] = json!(round4(completion));
            }
            task
        })
        .collect();
    let flakes: Vec<&str> = all
        .iter()
        .filter(|s| s.flake())
        .map(|s| s.task_id.as_str())
        .collect();
    let w = chrono::Duration::days(window_days);
    let alarm: Vec<String> = stats_window(rows, now - w, now)
        .into_iter()
        .filter(|s| s.tier == "regression" && s.pass_at_1() < 1.0)
        .map(|s| s.task_id)
        .collect();
    let mut report = json!({
        "no_data": all.is_empty(),
        "tiers": tier_json,
        "tasks": tasks,
        "flakes": flakes,
        "regression_alarm": alarm,
    });
    if let Some(n) = graduate_n {
        let candidates: Vec<String> = group_by_task(rows.iter().collect())
            .into_iter()
            .filter(|(_, trs)| {
                trs.last().map(|r| r.tier.as_str()) == Some("capability")
                    && trs.len() >= n
                    && trs[trs.len() - n..].iter().all(|r| r.pass)
            })
            .map(|(tid, _)| tid)
            .collect();
        report["graduation_eligible"] =
            Value::Array(candidates.into_iter().map(Value::String).collect());
    }
    report
}

fn modal_rev(rows: &[&Row]) -> Option<String> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for r in rows.iter().filter_map(|r| r.bank_rev.as_deref()) {
        *counts.entry(r).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by_key(|(rev, n)| (*n, std::cmp::Reverse(*rev)))
        .map(|(rev, _)| rev.to_string())
}

fn compare_variants(rows: &[Row], variant: &str) -> Value {
    let baseline: Vec<&Row> = rows
        .iter()
        .filter(|r| r.variant.as_deref().unwrap_or("baseline") == "baseline")
        .collect();
    let side: Vec<&Row> = rows
        .iter()
        .filter(|r| r.variant.as_deref() == Some(variant))
        .collect();
    let base_rev = modal_rev(&baseline);
    let var_rev = modal_rev(&side);
    fn at<'a>(rs: &[&'a Row], rev: &Option<String>) -> Vec<&'a Row> {
        rs.iter()
            .filter(|r| r.bank_rev.as_ref() == rev.as_ref())
            .cloned()
            .collect()
    }
    let mut by_task: BTreeMap<String, Vec<&Row>> = BTreeMap::new();
    for r in rows {
        by_task.entry(r.task_id.clone()).or_default().push(r);
    }
    let mut tasks = Map::new();
    let mut missing_in_variant = Vec::new();
    let mut missing_in_baseline = Vec::new();
    for (tid, task_rows) in &by_task {
        let b = at(
            &task_rows
                .iter()
                .filter(|r| r.variant.as_deref().unwrap_or("baseline") == "baseline")
                .cloned()
                .collect::<Vec<_>>(),
            &base_rev,
        );
        let v = at(
            &task_rows
                .iter()
                .filter(|r| r.variant.as_deref() == Some(variant))
                .cloned()
                .collect::<Vec<_>>(),
            &var_rev,
        );
        if b.is_empty() {
            missing_in_baseline.push(tid.clone());
        }
        if v.is_empty() {
            missing_in_variant.push(tid.clone());
        }
        if b.is_empty() || v.is_empty() {
            continue;
        }
        let (br, bp1, vr, vp1, delta, verdict) = pair_verdict(&b, &v);
        tasks.insert(
            tid.clone(),
            json!({
                "baseline": {"runs": br, "pass_at_1": round4(bp1)},
                "variant": {"runs": vr, "pass_at_1": round4(vp1)},
                "delta": round4(delta),
                "verdict": verdict,
            }),
        );
    }
    json!({
        "variant": variant,
        "tasks": tasks,
        "missing_in_variant": missing_in_variant,
        "missing_in_baseline": missing_in_baseline,
        "baseline_rev": base_rev,
        "variant_rev": var_rev,
    })
}

/// The summary document `evals_health_summary` reads: alarm, regressed set,
/// tier pass rate, flake count, and the staleness fold (newest
/// regression-tier timestamp vs the window). Staleness lives here so Python
/// carries the fields instead of re-folding them (d-b6cc1a2a).
fn summary_payload(history: &str, stale_days: i64, now: DateTime<Utc>) -> Value {
    let rows = read_rows(history, Some("baseline"), None);
    let (_, regressed) = trend_fold(&rows, stale_days, now);
    let w = chrono::Duration::days(stale_days);
    let recent_alarm: Vec<String> = stats_window(&rows, now - w, now)
        .into_iter()
        .filter(|s| s.tier == "regression" && s.pass_at_1() < 1.0)
        .map(|s| s.task_id)
        .collect();
    let fold = report_fold(&rows, stale_days, now, None, None);
    let reg = fold.get("tiers").and_then(|t| t.get("regression"));
    let pass_rate = reg
        .and_then(|t| t.get("pass_rate"))
        .cloned()
        .unwrap_or(json!(null));
    let flake_count = fold
        .get("flakes")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
    let reg_rows: Vec<&Row> = rows.iter().filter(|r| r.tier == "regression").collect();
    let never_ran = reg_rows.is_empty();
    let age_days =
        reg_rows.iter().filter_map(|r| r.ts).max().map(|newest| {
            ((now - newest).num_seconds() as f64 / 86_400.0 * 1000.0).round() / 1000.0
        });
    let stale = age_days.map_or(false, |age| age > stale_days as f64);
    json!({
        "regression_alarm": recent_alarm,
        "regressed": regressed,
        "regression_pass_rate": pass_rate,
        "flake_count": flake_count,
        "row_count": rows.len(),
        "never_ran": never_ran,
        "age_days": age_days,
        "stale": stale,
    })
}

fn print_summary(history: &str, stale_days: i64, now: DateTime<Utc>) -> i32 {
    println!(
        "{}",
        serde_json::to_string(&summary_payload(history, stale_days, now)).unwrap_or_default()
    );
    0
}

pub fn run_evals_trend(args: &[String]) -> i32 {
    if args.is_empty() {
        // stdin summary: a JSON payload on stdin (the verb_call shape),
        // `{"op": "summary", "history": "...", "stale_days": N}`; the flags
        // below remain the human surface.
        use std::io::Read;

        let mut buf = String::new();
        if std::io::stdin().read_to_string(&mut buf).is_ok() {
            if let Ok(v) = serde_json::from_str::<Value>(&buf) {
                if v.get("op").and_then(Value::as_str) == Some("summary") {
                    let history = v
                        .get("history")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let stale_days = v.get("stale_days").and_then(Value::as_i64).unwrap_or(7);
                    return print_summary(&history, stale_days, Utc::now());
                }
            }
        }
        eprintln!("{USAGE}");
        return EXIT_USAGE;
    }
    let mut history = String::new();
    let mut mode = String::from("report");
    let mut stale_days: i64 = 7;
    let mut since: Option<usize> = None;
    let mut graduate = false;
    let mut consecutive_n: usize = 3;
    let mut json_out = false;
    let mut compare: Option<String> = None;
    let mut planned: Option<BTreeMap<String, usize>> = None;
    let mut now: Option<DateTime<Utc>> = None;
    let mut i = 0usize;
    while i < args.len() {
        let arg = args[i].clone();
        i += 1;
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) => (n.to_string(), Some(v.to_string())),
            None => (arg.clone(), None),
        };
        // `--graduate` is bare in the documented surface; an integer argument
        // is the optional count. `--consecutive` sizes the count without
        // enabling the graduation view (the old CLI's shapes, both kept).
        if name == "--graduate" || name == "--consecutive" {
            if name == "--graduate" {
                graduate = true;
            }
            let had_inline = inline.is_some();
            let raw = match inline {
                Some(v) => Some(v),
                None => args.get(i).filter(|t| !t.starts_with('-')).cloned(),
            };
            if let Some(raw) = raw {
                match raw.parse::<usize>() {
                    Ok(v) => consecutive_n = v,
                    Err(_) => {
                        eprintln!("evals-trend: --graduate needs an integer");
                        return EXIT_USAGE;
                    }
                }
                if !had_inline {
                    i += 1;
                }
            }
            continue;
        }
        let mut value = |what: &str| -> Result<String, ()> {
            match inline.clone() {
                Some(v) => Ok(v),
                None => match args.get(i).cloned() {
                    Some(v) => {
                        i += 1;
                        Ok(v)
                    }
                    None => {
                        eprintln!("evals-trend: {what} needs a value");
                        Err(())
                    }
                },
            }
        };
        match name.as_str() {
            "--history" => match value("--history") {
                Ok(v) => history = v,
                Err(()) => return EXIT_USAGE,
            },
            "--mode" => match value("--mode") {
                Ok(v) => mode = v,
                Err(()) => return EXIT_USAGE,
            },
            "--stale-days" => match value("--stale-days").map(|v| v.parse::<i64>()) {
                Ok(Ok(v)) => stale_days = v,
                _ => {
                    eprintln!("evals-trend: --stale-days needs an integer");
                    return EXIT_USAGE;
                }
            },
            "--since" => match value("--since").map(|v| v.parse::<usize>()) {
                Ok(Ok(v)) => since = Some(v),
                _ => {
                    eprintln!("evals-trend: --since needs an integer");
                    return EXIT_USAGE;
                }
            },
            "--json" | "-J" => json_out = true,
            "--compare" => match value("--compare") {
                Ok(v) => compare = Some(v),
                Err(()) => return EXIT_USAGE,
            },
            "--planned" => match value("--planned")
                .and_then(|v| serde_json::from_str::<BTreeMap<String, usize>>(&v).map_err(|_| ()))
            {
                Ok(v) => planned = Some(v),
                Err(()) => {
                    eprintln!("evals-trend: --planned must be a JSON object of task_id -> expected attempts");
                    return EXIT_USAGE;
                }
            },
            "--now" => match value("--now").map(|v| DateTime::parse_from_rfc3339(&v)) {
                Ok(Ok(dt)) => now = Some(dt.with_timezone(&Utc)),
                _ => {
                    eprintln!("evals-trend: --now must be RFC3339");
                    return EXIT_USAGE;
                }
            },
            _ => {
                eprintln!("evals-trend: unknown flag {name}");
                eprintln!("{USAGE}");
                return EXIT_USAGE;
            }
        }
    }
    if history.is_empty() {
        eprintln!("evals-trend: --history is required");
        eprintln!("{USAGE}");
        return EXIT_USAGE;
    }
    let now = now.unwrap_or_else(Utc::now);

    if mode == "summary" {
        return print_summary(&history, stale_days, now);
    }

    if mode == "trend" {
        if !PathBuf::from(&history).exists() {
            println!("evals trend: no_data (no history yet)");
            return 0;
        }
        let rows = read_rows(&history, Some("baseline"), None);
        let (view, regressed) = trend_fold(&rows, stale_days, now);
        if json_out {
            println!(
                "{}",
                serde_json::to_string_pretty(&view).unwrap_or_default()
            );
        } else {
            println!(
                "Eval trend: prior since {}, recent since {}",
                view["prior_start"].as_str().unwrap_or_default(),
                view["recent_start"].as_str().unwrap_or_default()
            );
            if let Some(tasks) = view["tasks"].as_object() {
                for (tid, t) in tasks {
                    println!(
                        "  {tid}  prior {:.0}% ({})  recent {:.0}% ({})  delta={:+.2}  {}",
                        t["prior"]["pass_at_1"].as_f64().unwrap_or(0.0) * 100.0,
                        t["prior"]["runs"],
                        t["recent"]["pass_at_1"].as_f64().unwrap_or(0.0) * 100.0,
                        t["recent"]["runs"],
                        t["delta"].as_f64().unwrap_or(0.0),
                        t["verdict"].as_str().unwrap_or_default(),
                    );
                }
            }
            for (label, miss) in [
                ("prior", view["missing_in_prior"].clone()),
                ("recent", view["missing_in_recent"].clone()),
            ] {
                let ids: Vec<&str> = miss
                    .as_array()
                    .map(|a| a.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                if !ids.is_empty() {
                    println!("  missing in {label}: {}", ids.join(", "));
                }
            }
            let regressed_ids: Vec<&str> = view["regressed"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            if !regressed_ids.is_empty() {
                println!("  REGRESSED: {}", regressed_ids.join(", "));
            }
        }
        return if regressed.is_empty() { 0 } else { 4 };
    }

    // --mode report
    if let Some(v) = &compare {
        let ok = v == "baseline"
            || (v.starts_with('v') && v[1..].bytes().all(|b| b.is_ascii_digit()) && v.len() > 1);
        if !ok {
            eprintln!("Error: --compare must be 'baseline' or 'v<N>', got '{v}'");
            return 1;
        }
        let rows = read_rows(&history, None, since);
        let cmp = compare_variants(&rows, v);
        if json_out {
            println!("{}", serde_json::to_string_pretty(&cmp).unwrap_or_default());
        } else {
            if let Some(tasks) = cmp["tasks"].as_object() {
                for (tid, t) in tasks {
                    println!(
                        "  {tid}  baseline {:.0}% ({})  {} {:.0}% ({})  delta={:+.2}  {}",
                        t["baseline"]["pass_at_1"].as_f64().unwrap_or(0.0) * 100.0,
                        t["baseline"]["runs"],
                        cmp["variant"].as_str().unwrap_or_default(),
                        t["variant"]["pass_at_1"].as_f64().unwrap_or(0.0) * 100.0,
                        t["variant"]["runs"],
                        t["delta"].as_f64().unwrap_or(0.0),
                        t["verdict"].as_str().unwrap_or_default(),
                    );
                }
            }
            let variant_label = cmp["variant"].as_str().unwrap_or_default().to_string();
            for (label, miss) in [
                ("baseline".to_string(), cmp["missing_in_baseline"].clone()),
                (variant_label, cmp["missing_in_variant"].clone()),
            ] {
                let ids: Vec<&str> = miss
                    .as_array()
                    .map(|a| a.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                if !ids.is_empty() {
                    println!("  missing in {label}: {}", ids.join(", "));
                }
            }
            println!(
                "  diff: git diff {} {}",
                cmp["baseline_rev"].as_str().unwrap_or("None"),
                cmp["variant_rev"].as_str().unwrap_or("None")
            );
        }
        return 0;
    }

    if !PathBuf::from(&history).exists() {
        println!("evals report: no_data (no history yet)");
        return 0;
    }
    let rows = read_rows(&history, Some("baseline"), since);
    let report = report_fold(
        &rows,
        stale_days,
        now,
        graduate.then_some(consecutive_n),
        planned.as_ref(),
    );
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
        return if report["regression_alarm"]
            .as_array()
            .map_or(false, |a| !a.is_empty())
        {
            4
        } else {
            0
        };
    }
    if report["no_data"].as_bool().unwrap_or(false) {
        println!("evals report: no_data (no history yet)");
        return 0;
    }
    println!("Evals report:");
    if let Some(tiers) = report["tiers"].as_object() {
        for (tier, agg) in tiers {
            println!(
                "  {tier}: {}/{} pass ({:.0}%)",
                agg["passes"],
                agg["runs"],
                agg["pass_rate"].as_f64().unwrap_or(0.0) * 100.0
            );
        }
    }
    if let Some(tasks) = report["tasks"].as_array() {
        for t in tasks {
            let mark = if t["flake"].as_bool().unwrap_or(false) {
                "FLAKE"
            } else if t["pass_k"].as_bool().unwrap_or(false) {
                "PASS"
            } else {
                "FAIL"
            };
            println!(
                "    {:11} {}: pass@1={:.0}% pass^{}={} [{}]",
                t["tier"].as_str().unwrap_or_default(),
                t["task_id"].as_str().unwrap_or_default(),
                t["pass_at_1"].as_f64().unwrap_or(0.0) * 100.0,
                t["grade_den"],
                t["pass_k"],
                mark,
            );
        }
    }
    let flake_ids: Vec<&str> = report["flakes"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if !flake_ids.is_empty() {
        println!("  flakes: {}", flake_ids.join(", "));
    }
    let alarm_ids: Vec<&str> = report["regression_alarm"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if !alarm_ids.is_empty() {
        println!("  REGRESSION ALARM: {} below 100%", alarm_ids.join(", "));
    }
    if let Some(elig) = report.get("graduation_eligible").and_then(Value::as_array) {
        let ids: Vec<&str> = elig.iter().filter_map(Value::as_str).collect();
        if ids.is_empty() {
            println!("  graduation-eligible: none");
        } else {
            println!("  graduation-eligible: {}", ids.join(", "));
        }
    }
    if alarm_ids.is_empty() {
        0
    } else {
        4
    }
}

#[cfg(test)]
mod tests;
