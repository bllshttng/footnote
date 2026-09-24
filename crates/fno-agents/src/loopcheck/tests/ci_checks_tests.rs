use super::*;

#[test]
fn ci_conclusion_failure_extracts_name() {
    let checks = serde_json::json!([
        {"name": "unit-tests", "state": "FAILURE", "bucket": "fail"}
    ]);
    let result = compute_ci_conclusion(&checks).unwrap();
    assert_eq!(
        result,
        CiConclusion::Failure(Some("unit-tests".to_string()))
    );
    let rendered = result.render();
    assert!(rendered.starts_with("FAILURE:"), "got: {rendered}");
    assert!(rendered.contains("unit-tests"), "got: {rendered}");
}

/// A cancelled check is a failure, and a skipping sibling never masks it.
#[test]
fn ci_conclusion_cancel_is_failure() {
    let checks = serde_json::json!([
        {"name": "ci", "state": "SUCCESS", "bucket": "pass"},
        {"name": "deploy", "state": "CANCELLED", "bucket": "cancel"}
    ]);
    assert_eq!(
        compute_ci_conclusion(&checks).unwrap(),
        CiConclusion::Failure(Some("deploy".to_string()))
    );
}

/// pass + skipping rolls up green; a pending bucket blocks it.
#[test]
fn ci_conclusion_bucket_vocabulary() {
    let green = serde_json::json!([
        {"name": "ci", "state": "SUCCESS", "bucket": "pass"},
        {"name": "publish", "state": "SKIPPED", "bucket": "skipping"}
    ]);
    assert_eq!(
        compute_ci_conclusion(&green).unwrap(),
        CiConclusion::Success
    );

    let pending = serde_json::json!([
        {"name": "ci", "state": "SUCCESS", "bucket": "pass"},
        {"name": "smoke", "state": "IN_PROGRESS", "bucket": "pending"}
    ]);
    assert_eq!(
        compute_ci_conclusion(&pending).unwrap(),
        CiConclusion::Pending
    );
}

/// An unknown or missing bucket fails closed as Pending, never green.
#[test]
fn ci_conclusion_unknown_bucket_fails_closed() {
    let unknown = serde_json::json!([
        {"name": "ci", "state": "SUCCESS", "bucket": "mystery"}
    ]);
    assert_eq!(
        compute_ci_conclusion(&unknown).unwrap(),
        CiConclusion::Pending
    );

    let missing = serde_json::json!([{"name": "ci", "state": "SUCCESS"}]);
    assert_eq!(
        compute_ci_conclusion(&missing).unwrap(),
        CiConclusion::Pending
    );
}

#[test]
fn ci_conclusion_empty_returns_none() {
    let checks = serde_json::json!([]);
    let result = compute_ci_conclusion(&checks).unwrap();
    assert_eq!(result, CiConclusion::None);
    assert_eq!(result.render(), "none");
}

#[test]
fn ci_conclusion_all_success() {
    let checks = serde_json::json!([
        {"name": "ci", "state": "SUCCESS", "bucket": "pass"}
    ]);
    let result = compute_ci_conclusion(&checks).unwrap();
    assert_eq!(result, CiConclusion::Success);
    assert_eq!(result.render(), "SUCCESS");
}

#[test]
fn failing_check_names_collects_fail_and_cancel_only() {
    let checks = serde_json::json!([
        {"name": "smoke",        "bucket": "fail"},
        {"name": "loc-ratchet",  "bucket": "pass"},
        {"name": "prompt-drift", "bucket": "cancel"},
        {"name": "self-test",    "bucket": "pending"},
        {"name": "doc-colo",     "bucket": "skipping"},
    ]);
    let mut got = failing_check_names(&checks);
    got.sort();
    assert_eq!(got, vec!["prompt-drift".to_string(), "smoke".to_string()]);
}

#[test]
fn failing_check_names_empty_when_all_green() {
    let checks = serde_json::json!([{"name": "smoke", "bucket": "pass"}]);
    assert!(failing_check_names(&checks).is_empty());
    // Malformed input never panics, yields empty.
    assert!(failing_check_names(&serde_json::json!({})).is_empty());
}

#[test]
fn coverage_statuses_are_not_generic_ci_checks() {
    let checks = serde_json::json!([
        {"name": "ci", "bucket": "pass"},
        {"name": "fno/review-coverage", "bucket": "pending"},
        {"name": "fno/review-coverage-unavailable", "bucket": "pending"}
    ]);
    let filtered = without_coverage_statuses(&checks);
    assert_eq!(filtered.as_array().unwrap().len(), 1);
    assert_eq!(
        compute_ci_conclusion(&filtered).unwrap(),
        CiConclusion::Success
    );
    assert!(failing_check_names(&filtered).is_empty());
    assert!(!ci_has_pending_checks(&filtered));
}

#[test]
fn ci_has_pending_gates_partial_ci() {
    // One check failed while another still runs -> pending (must hold, not
    // terminate: the pending job could be the session's own new red).
    let partial = serde_json::json!([
        {"name": "smoke",   "bucket": "fail"},
        {"name": "rust-ci", "bucket": "pending"},
    ]);
    assert!(ci_has_pending_checks(&partial));
    // Fully settled red -> no pending -> eligible for the terminal.
    let settled = serde_json::json!([
        {"name": "smoke",   "bucket": "fail"},
        {"name": "rust-ci", "bucket": "pass"},
        {"name": "doc",     "bucket": "skipping"},
    ]);
    assert!(!ci_has_pending_checks(&settled));
    // Unrecognized bucket is treated as pending (fail safe).
    let unknown = serde_json::json!([{"name": "x", "bucket": "queued"}]);
    assert!(ci_has_pending_checks(&unknown));
    // Malformed input never panics.
    assert!(!ci_has_pending_checks(&serde_json::json!({})));
}

#[test]
fn ci_has_pending_counts_latest_cancel_as_unresolved() {
    // A cancelled latest run produced no result: the DoneAwaitingMerge
    // terminal must not fire off it, exactly as it must not off a pending
    // one. Composition mirrors the real path (dedup, then the reader).
    let cancelled = serde_json::json!([
        {"name": "ci", "bucket": "cancel", "startedAt": "2026-08-15T00:00:00Z"}
    ]);
    assert!(ci_has_pending_checks(&latest_per_name(&cancelled)));
}

#[test]
fn latest_per_name_drops_superseded_cancel_for_newer_pass() {
    // The dedup is what makes the non-terminal cancel safe: a superseded
    // cancel from an earlier push loses to the newer same-name pass, so
    // the terminal is held only by a cancel that IS the latest run.
    let checks = serde_json::json!([
        {"name": "ci", "bucket": "cancel", "startedAt": "2026-08-15T00:00:00Z"},
        {"name": "ci", "bucket": "pass",  "startedAt": "2026-08-15T01:00:00Z"},
        {"name": "doc", "bucket": "skipping", "startedAt": "2026-08-15T01:00:00Z"},
    ]);
    let deduped = latest_per_name(&checks);
    assert!(!ci_has_pending_checks(&deduped));
    assert!(failing_check_names(&deduped).is_empty());
    // Recency keys on TRIGGER time: a slow superseded pass completing
    // later must not displace a newer fast fail.
    let swapped = serde_json::json!([
        {"name": "ci", "bucket": "pass", "startedAt": "2026-08-15T01:00:00Z"},
        {"name": "ci", "bucket": "cancel", "startedAt": "2026-08-15T00:00:00Z"},
    ]);
    let deduped = latest_per_name(&swapped);
    assert!(!ci_has_pending_checks(&deduped));
    assert!(failing_check_names(&deduped).is_empty());
}

#[test]
fn latest_per_name_tie_keeps_fail_over_same_time_pass() {
    // Parity with the Python rule: on a missing/equal timestamp a fail is
    // never dropped by a same-time non-fail, so a superseded pass can
    // never hide a real fail when the payload carries no ordering.
    let tie = serde_json::json!([
        {"name": "ci", "bucket": "pass"},
        {"name": "ci", "bucket": "fail"},
    ]);
    let deduped = latest_per_name(&tie);
    assert_eq!(failing_check_names(&deduped), vec!["ci".to_string()]);
}

#[test]
fn latest_per_name_keeps_same_name_checks_from_different_workflows() {
    // codex P1: several workflows in this repo define a job literally
    // named "self-test". A name-only key folds a cancelled self-test
    // from one workflow into a passing self-test from another, hiding
    // the real failure behind an unrelated workflow's pass. The key
    // must be (name, workflow), so both entries survive the dedup.
    let checks = serde_json::json!([
        {"name": "self-test", "bucket": "cancel", "workflow": "control-plane-doc-colocation",
         "startedAt": "2026-08-15T01:00:00Z"},
        {"name": "self-test", "bucket": "pass", "workflow": "loc-ratchet",
         "startedAt": "2026-08-15T00:00:00Z"},
    ]);
    let deduped = latest_per_name(&checks);
    assert_eq!(deduped.as_array().unwrap().len(), 2);
    assert!(ci_has_pending_checks(&deduped));
    assert_eq!(failing_check_names(&deduped), vec!["self-test".to_string()]);
}
