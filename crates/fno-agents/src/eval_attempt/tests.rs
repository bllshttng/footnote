use super::{classify, classify_rows, run_evals_attempt, validate_cohorts};
use serde_json::json;

fn decl() -> serde_json::Value {
    json!({
        "train": ["t1", "t2"],
        "validation": ["v1"],
        "qualification": ["q1", "q2"],
    })
}

#[test]
fn cohorts_valid_split_passes() {
    let (ok, errors, roles) = validate_cohorts(&decl(), None, None);
    assert!(ok, "unexpected errors: {errors:?}");
    assert!(errors.is_empty());
    assert!(roles.is_empty());
}

#[test]
fn cohorts_unknown_id_is_refused() {
    let known = json!(["t1", "v1", "q1"]);
    let (ok, errors, _) = validate_cohorts(&decl(), Some(&known), None);
    assert!(!ok);
    assert!(errors
        .iter()
        .any(|e| e.contains("t2") && e.contains("unknown")));
    assert!(errors
        .iter()
        .any(|e| e.contains("q2") && e.contains("unknown")));
}

#[test]
fn cohorts_overlap_is_refused() {
    let mut bad = decl();
    bad["qualification"] = json!(["q1", "t1"]);
    let (ok, errors, _) = validate_cohorts(&bad, None, None);
    assert!(!ok);
    assert!(errors
        .iter()
        .any(|e| e.contains("'t1'") && e.contains("both")));
}

#[test]
fn cohorts_duplicate_within_role_is_refused() {
    let mut bad = decl();
    bad["train"] = json!(["t1", "t1"]);
    let (ok, errors, _) = validate_cohorts(&bad, None, None);
    assert!(!ok);
    assert!(errors.iter().any(|e| e.contains("twice")));
}

#[test]
fn cohorts_missing_role_list_is_refused() {
    let bad = json!({ "train": ["t1"] });
    let (ok, errors, _) = validate_cohorts(&bad, None, None);
    assert!(!ok);
    assert!(errors
        .iter()
        .any(|e| e.contains("missing") && e.contains("qualification")));
}

#[test]
fn cohorts_non_string_id_is_refused() {
    let bad = json!({ "train": [1], "validation": [], "qualification": [] });
    let (ok, errors, _) = validate_cohorts(&bad, None, None);
    assert!(!ok);
    assert!(errors.iter().any(|e| e.contains("non-string")));
}

#[test]
fn cohorts_non_object_declaration_is_refused() {
    let (ok, errors, _) = validate_cohorts(&json!([1, 2]), None, None);
    assert!(!ok);
    assert_eq!(errors.len(), 1);
}

#[test]
fn cohorts_role_resolution_covers_all_and_unknown() {
    let interest = json!(["t1", "q2", "zz"]);
    let (_, errors, roles) = validate_cohorts(&decl(), None, Some(&interest));
    assert!(errors.is_empty());
    assert_eq!(roles.get("t1"), Some(&json!("train")));
    assert_eq!(roles.get("q2"), Some(&json!("qualification")));
    assert_eq!(roles.get("zz"), Some(&json!(null)));
}

#[test]
fn cli_cohorts_mode_prints_ok_verdict() {
    let args: Vec<String> = vec![
        "--cohorts".into(),
        decl().to_string(),
        "--task-ids".into(),
        json!(["t1"]).to_string(),
    ];
    assert_eq!(run_evals_attempt(&args), 0);
}

#[test]
fn cli_cohorts_mode_still_enforces_single_mode() {
    let both: Vec<String> = vec![
        "--cohorts".into(),
        decl().to_string(),
        "--rows".into(),
        "x.jsonl".into(),
    ];
    assert_eq!(run_evals_attempt(&both), 2);
}

fn obs(extra: serde_json::Value) -> serde_json::Value {
    json!({ "obs": extra })
}

#[test]
fn fixture_failure_is_infrastructure_and_retryable() {
    let v = classify(
        &obs(json!({ "fixture_prepared": false, "worker_required": true })),
        None,
    );
    assert_eq!(v.status, "infrastructure");
    assert!(v.retryable);
    assert_eq!(v.graded, None);
}

#[test]
fn spawn_refusal_is_unavailable_not_a_task_failure() {
    let v = classify(
        &obs(json!({
            "fixture_prepared": true, "worker_required": true,
            "worker_started": false,
        })),
        None,
    );
    assert_eq!(v.status, "unavailable");
    assert!(v.retryable);
}

#[test]
fn timeout_and_cancellation_are_unavailable() {
    for field in ["worker_timed_out", "cancelled"] {
        let v = classify(
            &obs(json!({
                "fixture_prepared": true, "worker_required": true,
                "worker_started": true, field: true,
                "grader_ran": false,
            })),
            None,
        );
        assert_eq!(v.status, "unavailable", "field {field}");
        assert!(v.retryable);
    }
}

#[test]
fn gate_blocked_is_unavailable() {
    let v = classify(
        &obs(json!({ "fixture_prepared": true, "worker_required": true, "gate_blocked": true })),
        None,
    );
    assert_eq!(v.status, "unavailable");
    assert!(v.retryable);
}

#[test]
fn graded_pass_is_final_and_not_retryable() {
    let v = classify(
        &obs(json!({
            "fixture_prepared": true, "worker_required": true,
            "worker_started": true, "grader_ran": true, "grader_passed": true,
        })),
        None,
    );
    assert_eq!(v.status, "graded");
    assert_eq!(v.graded, Some(true));
    assert!(!v.retryable);
}

#[test]
fn graded_task_failure_is_distinct_from_infrastructure() {
    // AC1-EDGE: the worker did the task wrong - a valid grade of false, never
    // an infrastructure/unavailable retryable row.
    let v = classify(
        &obs(json!({
            "fixture_prepared": true, "worker_required": true,
            "worker_started": true, "grader_ran": true, "grader_passed": false,
        })),
        None,
    );
    assert_eq!(v.status, "graded");
    assert_eq!(v.graded, Some(false));
    assert!(!v.retryable);
}

#[test]
fn worker_ran_without_grader_result_is_ungraded() {
    let v = classify(
        &obs(json!({
            "fixture_prepared": true, "worker_required": true,
            "worker_started": true, "grader_ran": true,
        })),
        None,
    );
    assert_eq!(v.status, "ungraded");
    assert!(v.retryable);
}

#[test]
fn grader_never_ran_is_ungraded() {
    let v = classify(
        &obs(json!({
            "fixture_prepared": true, "worker_required": true,
            "worker_started": true, "grader_ran": false,
        })),
        None,
    );
    assert_eq!(v.status, "ungraded");
    assert!(v.retryable);
}

#[test]
fn grade_only_task_grades_the_fixture() {
    let v = classify(
        &obs(
            json!({ "fixture_prepared": true, "worker_required": false, "grader_ran": true, "grader_passed": true }),
        ),
        None,
    );
    assert_eq!(v.status, "graded");
    assert_eq!(v.graded, Some(true));
}

#[test]
fn legacy_row_without_obs_is_never_guessed() {
    // AC1-EDGE: a pre-attempt row carries pass:false, but there is no
    // structured evidence - the classifier must not reinterpret the boolean.
    let v = classify(
        &json!({ "task_id": "t", "pass": false, "reason": "spawn exit 1" }),
        None,
    );
    assert_eq!(v.status, "legacy");
    assert_eq!(v.graded, None);
    assert!(!v.retryable);
}

#[test]
fn insufficient_obs_is_legacy() {
    // fixture prepared, grade-only, no grader evidence: supports no verdict.
    let v = classify(
        &obs(json!({ "fixture_prepared": true, "worker_required": false })),
        None,
    );
    assert_eq!(v.status, "legacy");
    assert!(!v.retryable);
}

#[test]
fn worker_required_without_start_evidence_is_unavailable() {
    // A crashed writer left partial obs for a worker attempt: the attempt
    // verifiably produced no grade, and the safe reading is retryable.
    let v = classify(
        &obs(json!({ "fixture_prepared": true, "worker_required": true })),
        None,
    );
    assert_eq!(v.status, "unavailable");
    assert!(v.retryable);
}

#[test]
fn expected_rev_drives_rev_match() {
    let row = obs(
        json!({ "fixture_prepared": true, "worker_required": false, "grader_ran": true, "grader_passed": true }),
    );
    let mut with_rev = row.clone();
    with_rev["bank_rev"] = json!("abc123");
    assert_eq!(classify(&with_rev, Some("abc123")).rev_match, Some(true));
    assert_eq!(classify(&with_rev, Some("def456")).rev_match, Some(false));
    // No bank_rev on the row: never matches a pinned revision.
    assert_eq!(classify(&row, Some("abc123")).rev_match, Some(false));
    // No expectation supplied: the field stays null, not false.
    assert_eq!(classify(&with_rev, None).rev_match, None);
}

#[test]
fn classify_rows_numbers_lines_and_tolerates_corrupt_rows() {
    let text = concat!(
        "{\"obs\":{\"fixture_prepared\":true,\"worker_required\":false,\"grader_ran\":true,\"grader_passed\":true}}\n",
        "{broken json\n",
        "{\"pass\": false}\n",
    );
    let out = classify_rows(text, None);
    let arr = out.as_array().unwrap();
    assert_eq!(arr.len(), 3);
    assert_eq!(arr[0]["line"], 1);
    assert_eq!(arr[0]["status"], "graded");
    assert_eq!(arr[1]["line"], 2);
    assert_eq!(arr[1]["status"], "legacy");
    assert_eq!(arr[2]["line"], 3);
    assert_eq!(arr[2]["status"], "legacy");
}

#[test]
fn cli_row_json_prints_one_verdict() {
    let args: Vec<String> = vec![
        "--row-json".into(),
        obs(json!({ "fixture_prepared": false })).to_string(),
    ];
    // Cannot capture println here; assert the exit code and rely on the
    // classify tests for the payload.
    assert_eq!(run_evals_attempt(&args), 0);
}

#[test]
fn cli_requires_exactly_one_input() {
    assert_eq!(run_evals_attempt(&[]), 2);
    let both: Vec<String> = vec![
        "--row-json".into(),
        "{}".into(),
        "--rows".into(),
        "x.jsonl".into(),
    ];
    assert_eq!(run_evals_attempt(&both), 2);
    assert_eq!(run_evals_attempt(&["--bogus".into()]), 2);
}

#[test]
fn cli_rows_missing_file_is_usage() {
    assert_eq!(
        run_evals_attempt(&[
            "--rows".into(),
            "/nonexistent/eval-attempt-probe.jsonl".into()
        ]),
        2
    );
}

#[test]
fn cli_bad_row_json_is_usage() {
    assert_eq!(run_evals_attempt(&["--row-json".into(), "{nope".into()]), 2);
}
