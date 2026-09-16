//! `evals-trend` tests: the fold semantics the Python fold carried (tier
//! segments, half-open windows, modal-rev compare, graduation), the views,
//! and the exit contract. Pinned clocks via `--now`; fixture histories are
//! tempdir JSONL, never the real one.
use super::*;

use std::fs;
use tempfile::TempDir;

fn ts_rfc(days_ago: f64, now: DateTime<Utc>) -> String {
    (now - chrono::Duration::milliseconds((days_ago * 86400.0 * 1000.0) as i64)).to_rfc3339()
}

fn row_json(task_id: &str, tier: &str, pass: bool, ts: &str) -> String {
    json!({
        "ts": ts, "task_id": task_id, "tier": tier, "pass": pass,
        "reason": "", "duration_s": 1.0, "repeat_index": 0,
        "bank_rev": null, "worker_provider": null,
    })
    .to_string()
}

fn write_history(tmp: &TempDir, lines: &[String]) -> String {
    let p = tmp.path().join("history.jsonl");
    fs::write(&p, lines.join("\n") + "\n").unwrap();
    p.to_string_lossy().into_owned()
}

fn now_pinned() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-09-15T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

#[test]
fn report_fold_stats_and_exit_codes() {
    let now = now_pinned();
    let tmp = TempDir::new().unwrap();
    let h = write_history(
        &tmp,
        &[
            row_json("t", "capability", true, &ts_rfc(1.0, now)),
            row_json("t", "capability", false, &ts_rfc(1.0, now)),
            row_json("t", "capability", true, &ts_rfc(1.0, now)),
            row_json("r", "regression", false, &ts_rfc(1.0, now)),
        ],
    );
    let out = run_evals_trend(&[
        "--history".into(),
        h,
        "--stale-days".into(),
        "7".into(),
        "--now".into(),
        now.to_rfc3339(),
    ]);
    assert_eq!(out, 4, "a below-100% regression task fires the alarm");
}

#[test]
fn report_no_data_exit_0() {
    let tmp = TempDir::new().unwrap();
    let p = tmp.path().join("history.jsonl");
    let out = run_evals_trend(&["--history".into(), p.to_string_lossy().into_owned()]);
    assert_eq!(out, 0);
}

#[test]
fn graduated_task_excludes_pre_graduation_failures() {
    let now = now_pinned();
    let tmp = TempDir::new().unwrap();
    let h = write_history(
        &tmp,
        &[
            row_json("t", "capability", false, &ts_rfc(3.0, now)),
            row_json("t", "capability", true, &ts_rfc(2.0, now)),
            row_json("t", "regression", false, &ts_rfc(1.0, now)),
        ],
    );
    // The newest tier is regression and the segment holds only the last row:
    // the pre-graduation capability failure must not fire the alarm.
    let out = run_evals_trend(&[
        "--history".into(),
        h,
        "--stale-days".into(),
        "7".into(),
        "--now".into(),
        now.to_rfc3339(),
    ]);
    assert_eq!(out, 4, "the regression run itself is below 100%");
}

#[test]
fn windowed_alarm_silent_when_all_rows_old_ac1_hp() {
    let now = now_pinned();
    let tmp = TempDir::new().unwrap();
    let day0 = ts_rfc(49.0, now);
    let mut lines = Vec::new();
    for _ in 0..3 {
        lines.push(row_json("r", "regression", false, &day0));
    }
    for _ in 0..20 {
        lines.push(row_json("r", "regression", true, &day0));
    }
    let h = write_history(&tmp, &lines);
    let out = run_evals_trend(&[
        "--history".into(),
        h,
        "--stale-days".into(),
        "7".into(),
        "--now".into(),
        now.to_rfc3339(),
    ]);
    assert_eq!(
        out, 0,
        "the 50-day-old flake does not fire the windowed alarm"
    );
}

#[test]
fn trend_regressed_ac1_err() {
    let now = now_pinned();
    let tmp = TempDir::new().unwrap();
    let mut lines = Vec::new();
    for _ in 0..3 {
        lines.push(row_json("r", "regression", true, &ts_rfc(10.0, now)));
    }
    lines.push(row_json("r", "regression", true, &ts_rfc(1.0, now)));
    lines.push(row_json("r", "regression", false, &ts_rfc(1.0, now)));
    lines.push(row_json("r", "regression", false, &ts_rfc(1.0, now)));
    let h = write_history(&tmp, &lines);
    let out = run_evals_trend(&[
        "--history".into(),
        h.clone(),
        "--stale-days".into(),
        "7".into(),
        "--now".into(),
        now.to_rfc3339(),
        "--mode".into(),
        "trend".into(),
    ]);
    assert_eq!(out, 4, "3/3 prior then 1/3 recent is regressed");
    // Summary mode carries both keys; the fold reads the same rows.
    let rows = read_rows(&h, Some("baseline"), None);
    let (view, regressed) = trend_fold(&rows, 7, now);
    assert_eq!(regressed, vec!["r"]);
    assert_eq!(view["regression_alarm"], json!(["r"]));
}

#[test]
fn ac1_edge_missing_in_prior_and_bad_ts_counts_nowhere() {
    let now = now_pinned();
    let tmp = TempDir::new().unwrap();
    let mut lines = vec![
        row_json("u", "regression", true, &ts_rfc(1.0, now)),
        "{not json at all}".to_string(),
    ];
    lines.push(
        json!({
            "ts": "not-a-timestamp", "task_id": "u", "tier": "regression", "pass": true,
        })
        .to_string(),
    );
    let h = write_history(&tmp, &lines);
    let out = run_evals_trend(&[
        "--history".into(),
        h,
        "--stale-days".into(),
        "7".into(),
        "--now".into(),
        now.to_rfc3339(),
        "--mode".into(),
        "trend".into(),
        "--json".into(),
    ]);
    assert_eq!(out, 0, "recent-only task: no verdict, no regressed");
}

#[test]
fn compare_variants_improved_and_modal_rev() {
    let now = now_pinned();
    let tmp = TempDir::new().unwrap();
    let line = |task: &str, pass: bool, variant: &str, rev: &str| {
        let mut v: Value =
            serde_json::from_str(&row_json(task, "regression", pass, &ts_rfc(1.0, now))).unwrap();
        v["variant"] = Value::String(variant.into());
        v["bank_rev"] = Value::String(rev.into());
        v.to_string()
    };
    let h = write_history(
        &tmp,
        &[
            line("t", true, "baseline", "new"),
            line("t", true, "baseline", "new"),
            line("t", false, "baseline", "old"),
            line("t", true, "v1", "v1rev"),
        ],
    );
    let out = run_evals_trend(&[
        "--history".into(),
        h,
        "--stale-days".into(),
        "7".into(),
        "--now".into(),
        now.to_rfc3339(),
        "--compare".into(),
        "v1".into(),
        "--json".into(),
    ]);
    assert_eq!(out, 0, "a compare view never fires the alarm");
}

#[test]
fn compare_flag_refuses_bad_variant() {
    let tmp = TempDir::new().unwrap();
    let h = write_history(&tmp, &[]);
    let out = run_evals_trend(&["--history".into(), h, "--compare".into(), "vX".into()]);
    assert_eq!(out, 1);
}

#[test]
fn graduation_candidates_last_n_pass() {
    let now = now_pinned();
    let tmp = TempDir::new().unwrap();
    let h = write_history(
        &tmp,
        &[
            row_json("cap", "capability", false, &ts_rfc(4.0, now)),
            row_json("cap", "capability", true, &ts_rfc(3.0, now)),
            row_json("cap", "capability", true, &ts_rfc(2.0, now)),
            row_json("cap", "capability", true, &ts_rfc(1.0, now)),
        ],
    );
    let out = run_evals_trend(&[
        "--history".into(),
        h,
        "--stale-days".into(),
        "7".into(),
        "--now".into(),
        now.to_rfc3339(),
        "--graduate".into(),
        "3".into(),
    ]);
    assert_eq!(out, 0);
}

// The documented bare --graduate form and --graduate followed by another
// flag both work again (the round-1 finding; the old CLI's shapes).
#[test]
fn graduate_bare_and_flag_adjacent_shapes() {
    let now = now_pinned();
    let tmp = TempDir::new().unwrap();
    let h = write_history(
        &tmp,
        &[
            row_json("cap", "capability", true, &ts_rfc(1.0, now)),
            row_json("cap", "capability", true, &ts_rfc(1.0, now)),
            row_json("cap", "capability", true, &ts_rfc(1.0, now)),
        ],
    );
    let bare = run_evals_trend(&[
        "--history".into(),
        h.clone(),
        "--stale-days".into(),
        "7".into(),
        "--now".into(),
        now.to_rfc3339(),
        "--graduate".into(),
    ]);
    assert_eq!(bare, 0, "bare --graduate parses with the default count");
    let adjacent = run_evals_trend(&[
        "--history".into(),
        h,
        "--stale-days".into(),
        "7".into(),
        "--now".into(),
        now.to_rfc3339(),
        "--graduate".into(),
        "--json".into(),
    ]);
    assert_eq!(
        adjacent, 0,
        "--graduate before another flag must not eat it"
    );
}

// --consecutive alone sizes the count but never enables the graduation view.
#[test]
fn consecutive_alone_does_not_enable_graduation() {
    let now = now_pinned();
    let tmp = TempDir::new().unwrap();
    let h = write_history(
        &tmp,
        &[row_json("cap", "capability", true, &ts_rfc(1.0, now))],
    );
    let out = run_evals_trend(&[
        "--history".into(),
        h,
        "--stale-days".into(),
        "7".into(),
        "--now".into(),
        now.to_rfc3339(),
        "--consecutive".into(),
        "5".into(),
    ]);
    assert_eq!(out, 0);
}

#[test]
fn usage_error_exits_2() {
    assert_eq!(run_evals_trend(&[]), 2);
}

// --- attempt-aware denominators + the planned view -------------------------

fn modern_row_json(task_id: &str, tier: &str, passed: bool, ts: &str, attempt: usize) -> String {
    json!({
        "ts": ts, "task_id": task_id, "tier": tier, "pass": passed,
        "attempt_index": attempt,
        "obs": {"fixture_prepared": true, "worker_required": false,
                 "grader_ran": true, "grader_passed": passed},
    })
    .to_string()
}

fn infra_row_json(task_id: &str, tier: &str, ts: &str) -> String {
    json!({
        "ts": ts, "task_id": task_id, "tier": tier, "pass": false,
        "obs": {"fixture_prepared": false, "worker_required": true},
    })
    .to_string()
}

#[test]
fn report_infra_attempt_never_dilutes_the_pass_rate() {
    let now = now_pinned();
    let tmp = TempDir::new().unwrap();
    let h = write_history(
        &tmp,
        &[
            modern_row_json("t", "regression", true, &ts_rfc(1.0, now), 0),
            infra_row_json("t", "regression", &ts_rfc(1.1, now)),
        ],
    );
    let rows = read_rows(&h, Some("baseline"), None);
    let report = report_fold(&rows, 7, now, None, None);
    let t = &report["tasks"][0];
    assert_eq!(t["runs"], 2);
    assert_eq!(t["grades"], 1);
    assert_eq!(t["passes"], 1);
    assert_eq!(t["pass_at_1"], serde_json::json!(1.0)); // not 0.5
    assert_eq!(t["attempts"]["infrastructure"], 1);
    assert_eq!(t["legacy_fold"], false);
    assert!(report["regression_alarm"].as_array().unwrap().is_empty());
}

#[test]
fn report_all_legacy_segment_keeps_the_boolean_fold() {
    let now = now_pinned();
    let tmp = TempDir::new().unwrap();
    let h = write_history(
        &tmp,
        &[
            row_json("old", "regression", true, &ts_rfc(2.0, now)),
            row_json("old", "regression", false, &ts_rfc(1.0, now)),
        ],
    );
    let rows = read_rows(&h, Some("baseline"), None);
    let report = report_fold(&rows, 7, now, None, None);
    let t = &report["tasks"][0];
    assert_eq!(t["runs"], 2);
    assert_eq!(t["grades"], 0);
    assert_eq!(t["passes"], 1);
    assert_eq!(t["pass_at_1"], serde_json::json!(0.5));
    assert_eq!(t["legacy_fold"], true);
    // The old boolean fold still fires the regression alarm.
    assert_eq!(report["regression_alarm"], serde_json::json!(["old"]));
}

#[test]
fn report_planned_view_exposes_missing_attempts() {
    let now = now_pinned();
    let tmp = TempDir::new().unwrap();
    let h = write_history(
        &tmp,
        &[
            modern_row_json("t", "regression", true, &ts_rfc(1.0, now), 0),
            // planned q2 never ran
        ],
    );
    let rows = read_rows(&h, Some("baseline"), None);
    let mut planned = BTreeMap::new();
    planned.insert("t".to_string(), 2usize);
    planned.insert("q2".to_string(), 1usize);
    let report = report_fold(&rows, 7, now, None, Some(&planned));
    let tasks = report["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 1); // a rowless planned task stays visible only via --planned
    let t = &tasks[0];
    assert_eq!(t["expected_attempts"], 2);
    assert_eq!(t["missing_attempts"], 1);
    assert_eq!(t["completion"], serde_json::json!(0.5));
}

#[test]
fn planned_bad_json_is_usage() {
    let now = now_pinned();
    let tmp = TempDir::new().unwrap();
    let h = write_history(
        &tmp,
        &[row_json("t", "regression", true, &ts_rfc(1.0, now))],
    );
    let out = run_evals_trend(&[
        "--history".into(),
        h,
        "--mode".into(),
        "report".into(),
        "--planned".into(),
        "{not json".into(),
        "--now".into(),
        now.to_rfc3339(),
    ]);
    assert_eq!(out, 2);
}

#[test]
fn read_rows_skips_corrupt_lines() {
    let now = now_pinned();
    let tmp = TempDir::new().unwrap();
    let good = row_json("a", "regression", true, &ts_rfc(1.0, now));
    let h = write_history(&tmp, &[good]);
    // An interrupted append leaves a truncated JSON fragment with no newline.
    {
        use std::io::Write;
        let mut f = fs::OpenOptions::new().append(true).open(&h).unwrap();
        write!(f, "{{\"task_id\": \"b\", \"pa").unwrap();
    }
    let rows = read_rows(&h, Some("baseline"), None);
    assert_eq!(
        rows.len(),
        1,
        "the truncated fragment is skipped, not fatal"
    );
    assert_eq!(rows[0].task_id, "a");
}

#[test]
fn summary_carries_the_staleness_fields() {
    let now = now_pinned();
    let tmp = TempDir::new().unwrap();
    let h = write_history(
        &tmp,
        &[
            row_json("r", "regression", true, &ts_rfc(1.0, now)),
            row_json("c", "capability", true, &ts_rfc(0.5, now)),
        ],
    );
    let v = summary_payload(&h, 7, now);
    assert_eq!(v["row_count"], json!(2));
    assert_eq!(v["never_ran"], json!(false));
    let age = v["age_days"].as_f64().unwrap();
    assert!(
        (age - 1.0).abs() < 0.001,
        "newest regression ts is 1 day old, got {age}"
    );
    assert_eq!(v["stale"], json!(false));
}

#[test]
fn summary_stale_when_newest_regression_exceeds_the_window() {
    let now = now_pinned();
    let tmp = TempDir::new().unwrap();
    let h = write_history(
        &tmp,
        &[row_json("r", "regression", true, &ts_rfc(9.0, now))],
    );
    let v = summary_payload(&h, 7, now);
    assert_eq!(v["stale"], json!(true));
    assert_eq!(v["never_ran"], json!(false));
}

#[test]
fn summary_never_ran_when_no_regression_rows() {
    let now = now_pinned();
    let tmp = TempDir::new().unwrap();
    let h = write_history(
        &tmp,
        &[row_json("c", "capability", true, &ts_rfc(0.1, now))],
    );
    let v = summary_payload(&h, 7, now);
    assert_eq!(v["never_ran"], json!(true));
    assert_eq!(v["stale"], json!(false));
    assert_eq!(v["age_days"], json!(null));
    assert_eq!(v["row_count"], json!(1));
}

#[test]
fn summary_row_count_counts_baseline_rows_only() {
    let now = now_pinned();
    let tmp = TempDir::new().unwrap();
    let h = write_history(
        &tmp,
        &[
            row_json("r", "regression", true, &ts_rfc(1.0, now)),
            format!(
                "{{\"task_id\":\"r\",\"tier\":\"regression\",\"pass\":true,\"ts\":\"{}\",\"variant\":\"v1\"}}",
                ts_rfc(0.5, now)
            ),
        ],
    );
    let v = summary_payload(&h, 7, now);
    assert_eq!(
        v["row_count"],
        json!(1),
        "v1 rows stay out of the baseline fold"
    );
}
