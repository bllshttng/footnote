//! `--by-cohort` tests: cohorts stay apart, exclusions count by reason, and
//! cost reads measured only when every scored row has usage and a price line.
//! Tempdir histories, pinned clock via the fold's arguments.

use super::{cohort_fold, run_evals_trend};
use serde_json::{json, Value};
use std::fs;
use tempfile::TempDir;

const NOW: &str = "2026-09-15T12:00:00Z";

fn lane_row(
    task: &str,
    pass: bool,
    cohort: &str,
    status: &str,
    lane_status: &str,
    usage: Value,
    model: &str,
) -> String {
    let mut v = json!({
        "ts": NOW, "task_id": task, "tier": "capability",
        "pass": pass, "duration_s": 2.0, "status": status, "lane_status": lane_status,
        "experiment_id": cohort, "observed_model": model,
    });
    if !usage.is_null() {
        v["usage"] = usage;
    }
    v.to_string()
}

fn write_history(tmp: &TempDir, lines: &[String]) -> String {
    let p = tmp.path().join("history.jsonl");
    fs::write(&p, lines.join("\n") + "\n").unwrap();
    p.to_string_lossy().into_owned()
}

fn fold_of(tmp: &TempDir, lines: &[String]) -> Value {
    let h = write_history(tmp, lines);
    let rows = super::read_rows(&h, None, None);
    cohort_fold(&rows, None)
}

fn two_cohorts_kept_apart() {
    let tmp = TempDir::new().unwrap();
    let fold = fold_of(
        &tmp,
        &[
            lane_row(
                "t1",
                true,
                "cohort-a",
                "graded",
                "ok",
                json!({"input": 1, "output": 1}),
                "glm-5.3-flash",
            ),
            lane_row(
                "t2",
                true,
                "cohort-a",
                "graded",
                "ok",
                json!({"input": 1, "output": 1}),
                "glm-5.3-flash",
            ),
            lane_row(
                "t3",
                false,
                "cohort-a",
                "graded",
                "ok",
                json!(null),
                "glm-5.3-flash",
            ),
            lane_row(
                "t4",
                true,
                "cohort-b",
                "graded",
                "ok",
                json!(null),
                "glm-5.3-flash",
            ),
        ],
    );
    let a = &fold["cohorts"]["cohort-a"];
    let b = &fold["cohorts"]["cohort-b"];
    assert_eq!(a["attempts"], 3);
    assert_eq!(a["accepted"], 2);
    assert_eq!(b["attempts"], 1);
    assert_eq!(b["accepted"], 1);
    let rate = a["pass_rate"].as_f64().unwrap();
    let lo = a["pass_rate_ci95"][0].as_f64().unwrap();
    let hi = a["pass_rate_ci95"][1].as_f64().unwrap();
    assert!(
        lo <= rate && rate <= hi,
        "CI brackets the rate: {lo} {rate} {hi}"
    );
}

fn substituted_is_excluded_and_counted_never_scored() {
    let tmp = TempDir::new().unwrap();
    let fold = fold_of(
        &tmp,
        &[
            lane_row(
                "t1",
                true,
                "c",
                "graded",
                "ok",
                json!({"input": 1, "output": 1}),
                "m",
            ),
            lane_row(
                "t2",
                true,
                "c",
                "graded",
                "substituted",
                json!(null),
                "other",
            ),
            lane_row(
                "t3",
                false,
                "c",
                "unavailable",
                "unavailable",
                json!(null),
                "m",
            ),
        ],
    );
    let c = &fold["cohorts"]["c"];
    assert_eq!(c["accepted"], 1);
    assert_eq!(c["excluded"]["total"], 2);
    assert_eq!(c["lane_status_counts"]["substituted"], 1);
    // A row with no excluded_reason still lands in by_reason, under its lane
    // status or row status.
    assert_eq!(
        c["excluded"]["by_reason"],
        json!({"substituted": 1, "unavailable": 1})
    );
}

fn unmeasured_when_any_scored_row_lacks_usage() {
    let tmp = TempDir::new().unwrap();
    let fold = fold_of(
        &tmp,
        &[
            lane_row(
                "t1",
                true,
                "c",
                "graded",
                "ok",
                json!({"input": 1000000, "output": 1000000}),
                "m",
            ),
            lane_row("t2", true, "c", "graded", "ok", json!(null), "m"),
        ],
    );
    let c = &fold["cohorts"]["c"];
    assert_eq!(c["accepted"], 2);
    assert_eq!(c["cost"]["measured"], false);
    assert_eq!(c["excluded"]["usage_missing"], 1);
    assert_eq!(c["tokens"]["input"], 0);
}

fn measured_cost_with_prices() {
    let tmp = TempDir::new().unwrap();
    let h = write_history(
        &tmp,
        &[lane_row(
            "t1",
            true,
            "c",
            "graded",
            "ok",
            json!({"input": 1000000, "output": 1000000}),
            "glm",
        )],
    );
    let rows = super::read_rows(&h, None, None);
    let prices = json!({"glm": {"input_per_m": 1.0, "output_per_m": 2.0,
                                 "cache_read_per_m": 0.0, "cache_write_per_m": 0.0}});
    let fold = cohort_fold(&rows, Some(&prices));
    let c = &fold["cohorts"]["c"];
    assert_eq!(c["cost"]["measured"], true);
    assert_eq!(c["cost"]["dollars_total"], json!(3.0));
    assert_eq!(c["cost"]["dollars_per_accepted"], json!(3.0));
    // The CLI surface: --by-cohort defaults to --mode report, and malformed
    // --prices is a usage exit.
    let tmp = TempDir::new().unwrap();
    let h = write_history(
        &tmp,
        &[lane_row(
            "t1",
            true,
            "c",
            "graded",
            "ok",
            json!(null),
            "glm",
        )],
    );
    let ok = run_evals_trend(&["--history".into(), h.clone(), "--by-cohort".into()]);
    assert_eq!(ok, 0, "--by-cohort alone defaults to --mode report");
    let bad = run_evals_trend(&[
        "--history".into(),
        h,
        "--by-cohort".into(),
        "--prices".into(),
        "{not json".into(),
    ]);
    assert_eq!(bad, 2);
}

#[test]
fn all_contracts_in_one_declaration() {
    two_cohorts_kept_apart();
    substituted_is_excluded_and_counted_never_scored();
    unmeasured_when_any_scored_row_lacks_usage();
    measured_cost_with_prices();
}
