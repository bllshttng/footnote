//! `evals-qualification` tests: the honesty contract of the release
//! qualification projection. Pinned manifest + fixture rows, never the real
//! history.
use super::*;

use serde_json::Value;

const REV: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn qrow(cohort: &str, repeat: u64, graded: Option<bool>, rev: &str) -> Value {
    let mut obs = json!({"fixture_prepared": true, "worker_required": false});
    match graded {
        Some(passed) => {
            obs["grader_ran"] = json!(true);
            obs["grader_passed"] = json!(passed);
        }
        None => {
            obs["grader_ran"] = json!(false);
        }
    }
    json!({
        "task_id": "product-delivery-journey", "tier": "capability",
        "pass": graded == Some(true), "duration_s": 1.5,
        "repeat_index": repeat, "bank_rev": rev,
        "experiment_id": cohort, "obs": obs,
    })
}

fn base_manifest() -> Value {
    let scenario = |id: &str, class: &str| {
        json!({
            "id": id, "family": id, "bank_task": "product-delivery-journey",
            "repo": "footnote", "cohort": id, "cohort_class": class,
            "serving": if class == "conformance" { "grade-only" } else { "headless-worker" },
            "lane": null, "repeats": 2,
            "expected_cases": [format!("{id}/footnote/r1"), format!("{id}/footnote/r2")],
        })
    };
    json!({
        "manifest_version": 1,
        "release": {"product": "footnote", "footnote_version": "0.4.0",
                    "footnote_rev": REV, "bank_rev": REV, "declared_at": "2026-09-26"},
        "units": {"duration": "seconds", "effort": "minutes", "spend": "usd"},
        "bank_task": "product-delivery-journey",
        "comparison": {"repeats_per_scenario": 2, "runnable_repositories": ["footnote"],
                       "import_only_targets": [{"repository": "external-a", "reason": "no runner"}]},
        "scenarios": [
            scenario("install-first-use", "conformance"),
            scenario("delivery-evidence-failure", "conformance"),
            scenario("operator-effort-per-outcome", "live-trial"),
        ],
        "measurements": {
            "operator_active_minutes": {"unit": "minutes", "status": "not_measured", "source": null},
            "observed_spend_usd": {"unit": "usd", "status": "not_measured", "source": null},
        },
        "imports": [],
    })
}

fn scenario_field(p: &Value, id: &str, field: &str) -> Value {
    p.pointer(&format!("/qualification/scenarios/{id}/{field}"))
        .cloned()
        .unwrap()
}

#[test]
fn complete_conformance_set_passes_and_live_trial_stays_visible() {
    let rows = vec![
        qrow("install-first-use", 0, Some(true), REV),
        qrow("install-first-use", 1, Some(true), REV),
        qrow("delivery-evidence-failure", 0, Some(true), REV),
        qrow("delivery-evidence-failure", 1, Some(true), REV),
    ];
    let p = projection(&rows, &base_manifest());
    assert_eq!(
        scenario_field(&p, "install-first-use", "completed"),
        json!(2)
    );
    assert_eq!(scenario_field(&p, "install-first-use", "missing"), json!(0));
    assert!(p
        .pointer("/qualification/conformance_complete")
        .unwrap()
        .as_bool()
        .unwrap());
    // The unrun live trial is visible as missing, never as a pass.
    assert_eq!(
        scenario_field(&p, "operator-effort-per-outcome", "missing"),
        json!(2)
    );
    let not_measured = p
        .pointer("/qualification/not_measured")
        .unwrap()
        .as_array()
        .unwrap();
    assert_eq!(not_measured.len(), 2);
    assert_eq!(exit_code(&p), 0);
}

#[test]
fn missing_conformance_case_is_visible_and_fails_the_exit() {
    let rows = vec![qrow("install-first-use", 0, Some(true), REV)];
    let p = projection(&rows, &base_manifest());
    assert_eq!(scenario_field(&p, "install-first-use", "missing"), json!(1));
    assert!(!p
        .pointer("/qualification/conformance_complete")
        .unwrap()
        .as_bool()
        .unwrap());
    assert_eq!(exit_code(&p), 4);
}

#[test]
fn graded_failure_stays_failed_and_never_completes() {
    let rows = vec![
        qrow("install-first-use", 0, Some(false), REV),
        qrow("install-first-use", 1, Some(true), REV),
    ];
    let p = projection(&rows, &base_manifest());
    assert_eq!(scenario_field(&p, "install-first-use", "failed"), json!(1));
    assert_eq!(
        scenario_field(&p, "install-first-use", "completed"),
        json!(1)
    );
    assert_eq!(exit_code(&p), 4);
}

#[test]
fn wrong_revision_rows_land_in_the_unsupported_bucket() {
    let rows = vec![
        qrow(
            "install-first-use",
            0,
            Some(true),
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        ),
        qrow("install-first-use", 1, Some(true), REV),
    ];
    let p = projection(&rows, &base_manifest());
    assert_eq!(
        scenario_field(&p, "install-first-use", "unsupported"),
        json!(1)
    );
    assert_eq!(
        p.pointer("/qualification/revision_mismatch_rows")
            .unwrap()
            .as_u64()
            .unwrap(),
        1
    );
    assert_eq!(
        scenario_field(&p, "install-first-use", "completed"),
        json!(1)
    );
    assert_eq!(exit_code(&p), 4);
}

#[test]
fn legacy_rows_never_count_as_success() {
    let mut row = qrow("install-first-use", 0, None, REV);
    row.as_object_mut().unwrap().remove("obs"); // pre-attempt-era row
    let rows = vec![row, qrow("install-first-use", 1, Some(true), REV)];
    let p = projection(&rows, &base_manifest());
    assert_eq!(
        scenario_field(&p, "install-first-use", "unsupported"),
        json!(1)
    );
    assert_eq!(scenario_field(&p, "install-first-use", "missing"), json!(0));
    assert_eq!(
        scenario_field(&p, "install-first-use", "cases/r1"),
        json!("unsupported")
    );
    assert_eq!(exit_code(&p), 4);
}

#[test]
fn imported_false_success_is_reported_and_exit_fails() {
    let mut manifest = base_manifest();
    manifest["imports"] = json!([
        {"repository": "external-a", "tool_version": "1.2.3", "scenario": "install-first-use",
         "result": "pass", "false_success": true,
         "evidence": {"fixture": "imported-log", "observed_at": "2026-09-20"}}
    ]);
    let p = projection(&[], &manifest);
    let false_success = p
        .pointer("/qualification/false_success")
        .unwrap()
        .as_array()
        .unwrap();
    assert_eq!(false_success.len(), 1);
    // The import never merges into the runnable fold's counts.
    assert_eq!(scenario_field(&p, "install-first-use", "missing"), json!(2));
    assert_eq!(exit_code(&p), 4);
}

#[test]
fn undeclared_cohort_rows_stay_visible_and_fail_the_exit() {
    let rows = vec![qrow("rogue-cohort", 0, Some(true), REV)];
    let p = projection(&rows, &base_manifest());
    let undeclared = p
        .pointer("/qualification/undeclared_cohorts")
        .unwrap()
        .as_object()
        .unwrap();
    assert_eq!(undeclared.get("rogue-cohort"), Some(&json!(1)));
    assert_eq!(exit_code(&p), 4);
}

#[test]
fn unrunnable_history_answers_all_missing_not_an_error() {
    // An empty row list (missing history file folds the same way) is the
    // honest "nothing run" answer, not a failure to answer.
    let p = projection(&[], &base_manifest());
    let totals = p.pointer("/qualification/totals").unwrap();
    assert_eq!(totals["expected"], json!(6));
    assert_eq!(totals["missing"], json!(6));
}
