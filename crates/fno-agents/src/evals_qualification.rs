//! The release qualification projection: join the declared expected set
//! (`evals/fixtures/product-delivery/qualification.json`) against eval
//! history rows classified by the ONE native attempt classifier
//! (`eval_attempt::classify`, reused - never a second eligibility fold).
//!
//! Honesty rules: missing, failed, unsupported and false-success work stays
//! visible; nothing unrun is ever reported as a pass; live trials count as
//! measured only from graded rows; imported foreign observations stay in
//! their own provenance bucket, never merged into the runnable fold.

use std::collections::BTreeMap;

use serde_json::json;
use serde_json::Map;
use serde_json::Value;

use crate::eval_attempt::classify;

/// One scenario's evidence, accumulated per case (repeat) index.
#[derive(Default)]
struct ScenarioEvidence {
    completed: u64,
    failed: u64,
    missing: u64,
    unsupported: u64,
    unexpected_repeats: u64,
    duration_s: f64,
    attempts: Map<String, Value>,
    lane_statuses: Map<String, Value>,
    substituted: u64,
    cases: BTreeMap<u64, &'static str>,
}

/// Resolve one case from its rows' verdicts. Precedence: any valid passing
/// grade completes the case; else any valid failing grade fails it; else
/// wrong-revision/legacy evidence makes it unsupported; else missing.
fn resolve_case(completed: bool, failed: bool, unsupported: bool) -> &'static str {
    if completed {
        "completed"
    } else if failed {
        "failed"
    } else if unsupported {
        "unsupported"
    } else {
        "missing"
    }
}

fn bump(map: &mut Map<String, Value>, key: &str) {
    let entry = map.entry(key.to_string()).or_insert(json!(0));
    *entry = json!(entry.as_u64().unwrap_or(0) + 1);
}

fn add_u64(v: &mut Value, n: u64) {
    *v = json!(v.as_u64().unwrap_or(0) + n);
}

/// The qualification projection over raw history rows. Rows are selected per
/// scenario by `bank_task` + `experiment_id == cohort`; every verdict comes
/// from `eval_attempt::classify` against the manifest's pinned `bank_rev`.
pub fn projection(rows: &[Value], manifest: &Value) -> Value {
    let release = manifest.get("release").cloned().unwrap_or(json!({}));
    let units = manifest.get("units").cloned().unwrap_or(json!({}));
    let bank_rev = release
        .get("bank_rev")
        .and_then(Value::as_str)
        .unwrap_or("");
    let bank_task = manifest
        .get("bank_task")
        .and_then(Value::as_str)
        .unwrap_or("");
    let repeats_default = manifest
        .pointer("/comparison/repeats_per_scenario")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let mut scenario_out = Map::new();
    let mut totals =
        json!({"expected": 0, "completed": 0, "failed": 0, "missing": 0, "unsupported": 0});
    let mut conformance_complete = true;
    let mut declared: Vec<(String, String)> = Vec::new(); // (cohort, scenario id)
    let mut rev_mismatch_rows = 0u64;

    // A malformed manifest can never read as a passing qualification: no
    // declared version, no pinned bank revision, or no scenario at all makes
    // the projection answer "nothing declared", and "nothing declared" is
    // never conformance-complete.
    let version = manifest.get("manifest_version").and_then(Value::as_u64);
    let scenarios_list = manifest
        .get("scenarios")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let manifest_error = if version != Some(1) {
        Some(format!(
            "unsupported manifest_version {version:?}; expected 1"
        ))
    } else if bank_rev.is_empty() {
        Some("release.bank_rev pin is missing or empty".to_string())
    } else if scenarios_list.is_empty() {
        Some("no scenarios declared".to_string())
    } else {
        None
    };
    if manifest_error.is_some() {
        conformance_complete = false;
    }

    let scenarios = scenarios_list;
    for s in &scenarios {
        let id = s.get("id").and_then(Value::as_str).unwrap_or_default();
        let cohort = s.get("cohort").and_then(Value::as_str).unwrap_or(id);
        declared.push((cohort.to_string(), id.to_string()));
    }

    for s in &scenarios {
        let id = s.get("id").and_then(Value::as_str).unwrap_or_default();
        let cohort = s.get("cohort").and_then(Value::as_str).unwrap_or(id);
        let s_bank_task = s
            .get("bank_task")
            .and_then(Value::as_str)
            .unwrap_or(bank_task);
        let class = s.get("cohort_class").and_then(Value::as_str).unwrap_or("");
        let expected = s
            .get("expected_cases")
            .and_then(Value::as_array)
            .map(|a| a.len() as u64)
            .unwrap_or(0);
        let repeats = s
            .get("repeats")
            .and_then(Value::as_u64)
            .unwrap_or(repeats_default);

        let mut ev = ScenarioEvidence::default();
        // case_idx -> (completed, failed, unsupported)
        let mut per_case: BTreeMap<u64, (bool, bool, bool)> = BTreeMap::new();
        for r in rows {
            let r_task = r.get("task_id").and_then(Value::as_str).unwrap_or("");
            if r_task != s_bank_task {
                continue;
            }
            let r_cohort = r.get("experiment_id").and_then(Value::as_str).unwrap_or("");
            if r_cohort != cohort {
                continue;
            }
            ev.duration_s += r.get("duration_s").and_then(Value::as_f64).unwrap_or(0.0);
            if let Some(status) = r.get("lane_status").and_then(Value::as_str) {
                bump(&mut ev.lane_statuses, status);
            }
            if r.get("substituted") == Some(&Value::Bool(true)) {
                ev.substituted += 1;
            }
            let verdict = classify(r, Some(bank_rev));
            let r_rev = r.get("bank_rev").and_then(Value::as_str);
            let no_provenance = !bank_rev.is_empty() && r_rev.is_none();
            if verdict.rev_match == Some(false) || no_provenance {
                if verdict.rev_match == Some(false) {
                    rev_mismatch_rows += 1;
                }
                ev.unsupported += 1;
                let idx = r.get("repeat_index").and_then(Value::as_u64).unwrap_or(0);
                per_case.entry(idx).or_default().2 = true;
                continue;
            }
            if verdict.status == "legacy" {
                ev.unsupported += 1;
                let idx = r.get("repeat_index").and_then(Value::as_u64).unwrap_or(0);
                per_case.entry(idx).or_default().2 = true;
                continue;
            }
            bump(&mut ev.attempts, verdict.status);
            let idx = r.get("repeat_index").and_then(Value::as_u64).unwrap_or(0);
            let case = per_case.entry(idx).or_default();
            if verdict.status == "graded" {
                if verdict.graded == Some(true) {
                    case.0 = true;
                } else {
                    case.1 = true;
                }
            }
        }

        for idx in 0..repeats {
            let (completed, failed, unsupported) =
                per_case.get(&idx).copied().unwrap_or((false, false, false));
            let state = resolve_case(completed, failed, unsupported);
            ev.cases.insert(idx, state);
            match state {
                "completed" => ev.completed += 1,
                "failed" => ev.failed += 1,
                "unsupported" => {}
                _ => ev.missing += 1,
            }
        }
        for idx in per_case.keys() {
            if *idx >= repeats {
                ev.unexpected_repeats += 1;
            }
        }
        if class == "conformance" {
            let complete = ev.completed == expected && ev.failed == 0 && ev.unsupported == 0;
            conformance_complete &= complete;
        }

        let t = totals.as_object_mut().unwrap();
        add_u64(t.get_mut("expected").unwrap(), expected);
        add_u64(t.get_mut("completed").unwrap(), ev.completed);
        add_u64(t.get_mut("failed").unwrap(), ev.failed);
        add_u64(t.get_mut("missing").unwrap(), ev.missing);
        add_u64(t.get_mut("unsupported").unwrap(), ev.unsupported);

        let cases: Map<String, Value> = ev
            .cases
            .iter()
            .map(|(idx, state)| (format!("r{}", idx + 1), json!(state)))
            .collect();
        scenario_out.insert(
            id.to_string(),
            json!({
                "family": s.get("family").cloned().unwrap_or(json!(null)),
                "cohort_class": class,
                "serving": s.get("serving").cloned().unwrap_or(json!(null)),
                "repo": s.get("repo").cloned().unwrap_or(json!(null)),
                "expected": expected,
                "completed": ev.completed,
                "failed": ev.failed,
                "missing": ev.missing,
                "unsupported": ev.unsupported,
                "unexpected_repeats": ev.unexpected_repeats,
                "attempts": Value::Object(ev.attempts),
                "duration_s": (ev.duration_s * 1000.0).round() / 1000.0,
                "lane": {"statuses": Value::Object(ev.lane_statuses), "substituted": ev.substituted},
                "cases": Value::Object(cases),
            }),
        );
    }

    // Undeclared cohorts: rows tagged for this bank task with an experiment id
    // no scenario declared. They never count toward any scenario.
    let mut undeclared: Map<String, Value> = Map::new();
    for r in rows {
        let r_task = r.get("task_id").and_then(Value::as_str).unwrap_or("");
        if r_task != bank_task {
            continue;
        }
        let cohort = r
            .get("experiment_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if cohort.is_empty() || declared.iter().any(|(c, _)| *c == cohort) {
            continue;
        }
        bump(&mut undeclared, &cohort);
    }

    let imports = manifest
        .get("imports")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let false_success: Vec<Value> = imports
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|i| i.get("false_success") == Some(&Value::Bool(true)))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let measurements = manifest
        .get("measurements")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let not_measured: Vec<String> = measurements
        .as_object()
        .map(|m| {
            m.iter()
                .filter(|(_, v)| v.get("status").and_then(Value::as_str) == Some("not_measured"))
                .map(|(k, _)| k.clone())
                .collect()
        })
        .unwrap_or_default();

    json!({
        "qualification": {
            "manifest_version": manifest.get("manifest_version").cloned().unwrap_or(json!(null)),
            "manifest_error": manifest_error,
            "release": release,
            "revision_mismatch_rows": rev_mismatch_rows,
            "units": units,
            "scenarios": Value::Object(scenario_out),
            "totals": totals,
            "conformance_complete": conformance_complete,
            "measurements": measurements,
            "not_measured": not_measured,
            "imports": imports,
            "false_success": false_success,
            "undeclared_cohorts": Value::Object(undeclared),
        }
    })
}

/// Exit contract: 4 when conformance is incomplete, a false success is on
/// record, or undeclared cohort rows exist; 0 otherwise. A live-trial
/// scenario that has not run never fails the exit - it stays visible under
/// `missing` and `not_measured`.
pub fn exit_code(projection: &Value) -> i32 {
    let q = projection.get("qualification").unwrap_or(projection);
    let complete = q
        .get("conformance_complete")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let manifest_error = q.get("manifest_error").is_some_and(|e| !e.is_null());
    let false_success = q
        .get("false_success")
        .and_then(Value::as_array)
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    let undeclared = q
        .get("undeclared_cohorts")
        .and_then(Value::as_object)
        .map(|m| !m.is_empty())
        .unwrap_or(false);
    if complete && !manifest_error && !false_success && !undeclared {
        0
    } else {
        4
    }
}

/// Human-readable rendering of the projection (the non-JSON surface).
pub fn render_text(projection: &Value) -> String {
    let q = projection.get("qualification").unwrap_or(projection);
    let mut out = String::from("Qualification projection:\n");
    if let Some(err) = q.get("manifest_error").filter(|e| !e.is_null()) {
        out.push_str(&format!(
            "  MANIFEST ERROR: {}\n",
            err.as_str().unwrap_or("malformed manifest")
        ));
    }
    if let Some(scenarios) = q.get("scenarios").and_then(Value::as_object) {
        for (id, s) in scenarios {
            out.push_str(&format!(
                "  {id} ({}): {}/{} completed, {} failed, {} missing, {} unsupported\n",
                s.get("cohort_class").and_then(Value::as_str).unwrap_or("?"),
                s.get("completed").and_then(Value::as_u64).unwrap_or(0),
                s.get("expected").and_then(Value::as_u64).unwrap_or(0),
                s.get("failed").and_then(Value::as_u64).unwrap_or(0),
                s.get("missing").and_then(Value::as_u64).unwrap_or(0),
                s.get("unsupported").and_then(Value::as_u64).unwrap_or(0),
            ));
        }
    }
    let false_success: Vec<String> = q
        .get("false_success")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|i| {
                    i.get("scenario")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default();
    if !false_success.is_empty() {
        out.push_str(&format!("  FALSE SUCCESS: {}\n", false_success.join(", ")));
    }
    let undeclared: Vec<String> = q
        .get("undeclared_cohorts")
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    if !undeclared.is_empty() {
        out.push_str(&format!(
            "  UNDECLARED COHORTS: {}\n",
            undeclared.join(", ")
        ));
    }
    let not_measured: Vec<String> = q
        .get("not_measured")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if !not_measured.is_empty() {
        out.push_str(&format!("  not measured: {}\n", not_measured.join(", ")));
    }
    out
}

#[cfg(test)]
mod tests;
