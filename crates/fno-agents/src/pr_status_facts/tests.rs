//! AC1-AC9 for the status ops (the 2026-09-18 false readings).

use super::*;
use serde_json::json;
use std::cell::RefCell;

struct FakeGh {
    ok: bool,
    output: String,
    /// The answer for a `gh pr view --json reviewDecision` probe; the rules
    /// read and every other call get `output`.
    review_decision: String,
    calls: RefCell<Vec<Vec<String>>>,
}

impl GhProbe for FakeGh {
    fn run_gh(&self, _cwd: &Path, args: &[String]) -> Result<(bool, String, String), String> {
        self.calls.borrow_mut().push(args.to_vec());
        if args.contains(&"reviewDecision".to_string()) {
            return Ok((self.ok, self.review_decision.clone(), String::new()));
        }
        if args.iter().any(|a| a.contains("jobs?per_page=1")) {
            return Ok((self.ok, r#"{"total_count":0}"#.to_string(), String::new()));
        }
        Ok((self.ok, self.output.clone(), String::new()))
    }
}

fn rules_output(rules: Value) -> String {
    rules.to_string()
}

const REQUIRED_CHECKS_RULE: &str = r#"[
  {"type": "required_status_checks",
   "parameters": {"required_status_checks": [{"context": "smoke"}, {"context": "stacked-base-guard"}]}}
]"#;

// --- AC1-HP .. AC4-EDGE: merge_blocker --------------------------------------

#[test]
fn ac1_blocked_state_names_the_missing_required_check() {
    let probes = FakeGh {
        ok: true,
        output: rules_output(serde_json::from_str(REQUIRED_CHECKS_RULE).unwrap()),
        review_decision: String::new(),
        calls: RefCell::new(Vec::new()),
    };
    let payload = json!({
        "merge_state": "blocked",
        "base_ref": "main",
        "cwd": "/repo",
        "rollup": [
            {"name": "stacked-base-guard", "conclusion": "success"},
            {"name": "ci", "conclusion": "success"},
        ],
    });
    let out = merge_blocker(&probes, &payload);
    assert_eq!(out["blockers"], json!(["github_blocked"]));
    assert_eq!(out["missing_required_checks"], json!(["smoke"]));
    assert_eq!(out["state"], "blocked");
    // The gh read rode the injectable seam in the PR's cwd, with the base
    // ref named - gh fills owner/repo from the checkout.
    let calls = probes.calls.borrow();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].iter().any(|a| a.contains("rules/branches/main")));
}

#[test]
fn ac2_a_failed_rules_read_still_blocks_with_a_null_missing_list() {
    let probes = FakeGh {
        ok: false,
        output: "gh: HTTP 403".to_string(),
        review_decision: String::new(),
        calls: RefCell::new(Vec::new()),
    };
    let payload = json!({"merge_state": "blocked", "base_ref": "main", "cwd": "/repo"});
    let out = merge_blocker(&probes, &payload);
    assert_eq!(out["blockers"], json!(["github_blocked"]));
    assert!(out["missing_required_checks"].is_null());
    assert!(out["source"].as_str().unwrap().contains("failed"));
}

#[test]
fn ac3_the_states_existing_conjuncts_already_name_add_no_blocker() {
    for state in ["clean", "has_hooks", "unstable", "dirty", "unknown"] {
        let probes = FakeGh {
            ok: true,
            output: String::new(),
            review_decision: String::new(),
            calls: RefCell::new(Vec::new()),
        };
        let payload = json!({"merge_state": state, "base_ref": "main", "cwd": "/repo"});
        let out = merge_blocker(&probes, &payload);
        assert_eq!(
            out["blockers"],
            json!([]),
            "state {state} must not add a github_ blocker"
        );
        assert!(probes.calls.borrow().is_empty(), "state {state} read rules");
    }
    // A null merge_state (still computing) is the unknown arm too.
    let probes = FakeGh {
        ok: true,
        output: String::new(),
        review_decision: String::new(),
        calls: RefCell::new(Vec::new()),
    };
    let out = merge_blocker(&probes, &json!({"merge_state": null, "cwd": "/repo"}));
    assert_eq!(out["blockers"], json!([]));
}

#[test]
fn the_states_only_this_read_sees_get_their_named_blockers() {
    for (state, blocker) in [
        ("behind", "github_behind"),
        ("draft", "github_draft"),
        ("frobnicated", "github_merge_state_frobnicated"),
    ] {
        let probes = FakeGh {
            ok: true,
            output: String::new(),
            review_decision: String::new(),
            calls: RefCell::new(Vec::new()),
        };
        let out = merge_blocker(&probes, &json!({"merge_state": state, "cwd": "/repo"}));
        assert_eq!(out["blockers"], json!([blocker]), "state {state}");
    }
}

#[test]
fn a_required_review_rule_adds_required_review_to_the_missing_list() {
    let probes = FakeGh {
        ok: true,
        output: rules_output(json!([
            {"type": "pull_request", "parameters": {"required_approving_review_count": 2}},
        ])),
        review_decision: String::new(),
        calls: RefCell::new(Vec::new()),
    };
    let out = merge_blocker(
        &probes,
        &json!({"merge_state": "blocked", "base_ref": "main", "cwd": "/repo"}),
    );
    assert_eq!(out["blockers"], json!(["github_blocked"]));
    // No pr on the payload: the review probe cannot run, so the requirement
    // is named - the safe direction.
    assert_eq!(out["missing_required_checks"], json!(["required_review"]));
}

#[test]
fn a_green_status_row_satisfies_its_required_context() {
    // Commit statuses arrive in the context/state shape and uppercase.
    let probes = FakeGh {
        ok: true,
        output: rules_output(serde_json::from_str(REQUIRED_CHECKS_RULE).unwrap()),
        review_decision: String::new(),
        calls: RefCell::new(Vec::new()),
    };
    let payload = json!({
        "merge_state": "blocked",
        "base_ref": "main",
        "cwd": "/repo",
        "rollup": [
            {"context": "smoke", "state": "SUCCESS"},
            {"context": "stacked-base-guard", "state": "SUCCESS"},
        ],
    });
    let out = merge_blocker(&probes, &payload);
    assert_eq!(out["blockers"], json!(["github_blocked"]));
    // Every required context passed, so the rules read explains nothing.
    assert!(out["missing_required_checks"].is_null());
}

#[test]
fn a_pending_required_check_is_missing_not_satisfied() {
    let probes = FakeGh {
        ok: true,
        output: rules_output(serde_json::from_str(REQUIRED_CHECKS_RULE).unwrap()),
        review_decision: String::new(),
        calls: RefCell::new(Vec::new()),
    };
    let payload = json!({
        "merge_state": "blocked",
        "base_ref": "main",
        "cwd": "/repo",
        "rollup": [
            {"name": "smoke", "status": "IN_PROGRESS", "conclusion": ""},
            {"name": "stacked-base-guard", "conclusion": "success"},
        ],
    });
    let out = merge_blocker(&probes, &payload);
    assert_eq!(out["missing_required_checks"], json!(["smoke"]));
}

// --- AC5-HP .. AC9-HP: failure_cause ----------------------------------------

/// A raw Actions-log line: GitHub prefixes every line with an ISO timestamp.
fn at(min: u32, sec: u32, text: &str) -> String {
    format!("2026-09-18T13:{min:02}:{sec:02}.0000000Z {text}")
}

fn cross_door_log() -> String {
    [
        at(0, 0, "##[group]Run cargo test --lib --bins"),
        at(0, 1, "error: failed to remove directory /x/fno/lib: Directory not empty (os error 66)"),
        at(0, 5, "##[group]Run cargo test --test '*' --test-threads=1"),
        at(0, 6, "test every_removal_door_leaves_the_row_absent_from_all_three_stores ... FAILED"),
        at(0, 6, "thread 'every_removal_door_leaves_the_row_absent_from_all_three_stores' (1) panicked at tests/cross_door_property.rs:535:9:"),
        at(0, 6, "after all doors: the sweep reaped the live row"),
        at(0, 6, "note: this advisory line must not become the cause"),
        at(0, 6, "stack backtrace:"),
    ]
    .join("\n")
}

#[test]
fn ac5_the_window_scopes_the_block_and_the_panic_names_the_real_cause() {
    let payload = json!({
        "log": cross_door_log(),
        "window": ["2026-09-18T13:00:05Z", "2026-09-18T13:00:06Z"],
    });
    let out = failure_cause(&payload);
    assert_eq!(
        out["cause"],
        json!("test every_removal_door_leaves_the_row_absent_from_all_three_stores panicked at tests/cross_door_property.rs:535: after all doors: the sweep reaped the live row")
    );
    assert_eq!(out["source"], "cargo");
    assert_eq!(out["truncated"], false);
}

#[test]
fn ac6_without_a_window_the_cargo_verdict_still_beats_the_earlier_error_line() {
    let payload = json!({"log": cross_door_log()});
    let out = failure_cause(&payload);
    assert_eq!(out["source"], "cargo");
    assert!(
        out["cause"]
            .as_str()
            .unwrap()
            .starts_with("test every_removal_door_leaves_the_row_absent_from_all_three_stores"),
        "the passing test's stderr must not win: {}",
        out["cause"]
    );
}

#[test]
fn ac7_pytest_notes_sink_and_the_real_diagnostic_is_joined_in() {
    let log = [
        "##[group]Pytest (unit + integration)",
        "FAILED cli/tests/lint/test_check_pitfalls.py::test_preamble_ceiling - AssertionError",
        "E   AssertionError: note: entry Assert a positive marker, never an absence added: says 2026-07-27",
        "E   note: run scripts/ci/check-pitfalls.sh locally",
        "E   check-pitfalls: 3/10 entries, but the preamble is 105 B over the byte ceiling.",
        "step failed, stopping (fail-fast): Pytest (unit + integration)",
    ]
    .join("\n");
    let out = failure_cause(&json!({"log": log, "step": "Pytest (unit + integration)"}));
    let cause = out["cause"].as_str().unwrap();
    assert_eq!(out["source"], "pytest");
    // heal::pytest_nodeids strips a leading `cli/` so one repro runs from cli.
    assert!(cause.starts_with("FAILED tests/lint/test_check_pitfalls.py::test_preamble_ceiling: "));
    let cut = cause.find("105 B over the byte ceiling").unwrap();
    // The sunk note line is the LAST note text in the cause; the diagnostic
    // outranks it.
    assert!(cut < cause.rfind("note:").unwrap());
}

#[test]
fn ac8_an_error_shaped_line_is_the_fallback() {
    let log = [
        "=== lint ===",
        "src/x.py:12: error: cannot determine type",
        "step failed, stopping (fail-fast): lint",
    ]
    .join("\n");
    let out = failure_cause(&json!({"log": log, "step": "lint"}));
    assert_eq!(
        out["cause"],
        json!("src/x.py:12: error: cannot determine type")
    );
    assert_eq!(out["source"], "error_line");
}

#[test]
fn ac9_a_real_group_marker_opens_the_block_at_the_named_step() {
    let log = [
        "pre-group error: from an earlier step",
        "##[group]Sync + build",
        "in-step error: the real one",
        "step failed, stopping (fail-fast): Sync + build",
    ]
    .join("\n");
    let out = failure_cause(&json!({"log": log, "step": "Sync + build"}));
    assert_eq!(out["cause"], json!("in-step error: the real one"));
}

#[test]
fn a_build_script_panic_without_a_failed_line_is_named_alone() {
    let log = [
        at(0, 0, "thread 'build_script' panicked at build.rs:10:5:"),
        at(0, 0, "the registry file is unwritable"),
        at(0, 0, "stack backtrace:"),
    ]
    .join("\n");
    let out = failure_cause(&json!({"log": log}));
    assert_eq!(
        out["cause"],
        json!("panicked at build.rs:10: the registry file is unwritable")
    );
    assert_eq!(out["source"], "cargo");
}

#[test]
fn the_earliest_matching_error_pattern_wins_not_the_strongest() {
    let log = [
        "cli/src/x.py:52:1: E402 module level import not at top of file",
        "Found 1 error.",
        "step failed, stopping (fail-fast): lint",
    ]
    .join("\n");
    let out = failure_cause(&json!({"log": log, "step": "lint"}));
    assert_eq!(
        out["cause"],
        json!("cli/src/x.py:52:1: E402 module level import not at top of file")
    );
}

#[test]
fn an_oversized_cause_is_cut_and_the_cut_is_announced() {
    let long = format!(
        "test big ... FAILED\nthread 'big' panicked at a.rs:1:1:\n{}",
        "x".repeat(600)
    );
    let out = failure_cause(&json!({"log": long}));
    let cause = out["cause"].as_str().unwrap();
    assert_eq!(out["truncated"], true);
    assert!(cause.ends_with(" chars cut]"));
    assert!(cause.chars().count() < 600);
}

#[test]
fn an_empty_log_answers_a_null_cause_not_an_error() {
    let out = failure_cause(&json!({"log": ""}));
    assert!(out["cause"].is_null());
    assert_eq!(out["source"], "tail");
}

#[test]
fn a_markerless_log_with_a_named_step_still_scans_the_prefix() {
    // The block boundary is an optimization, never a precondition (the old
    // Python contract, kept): no group marker, so the whole prefix before
    // the step-failed line is the block.
    let log = [
        "error: deeper than any marker",
        "step failed, stopping (fail-fast): Ghost step",
    ]
    .join("\n");
    let out = failure_cause(&json!({"log": log, "step": "Ghost step"}));
    assert_eq!(out["cause"], json!("error: deeper than any marker"));
}

#[test]
fn the_step_failed_line_itself_never_joins_the_block() {
    let log = "boom error: x\nstep failed, stopping fail-fast: s\n".to_string();
    let out = failure_cause(&json!({"log": log, "step": "s"}));
    assert_eq!(out["cause"], json!("boom error: x"));
}

#[test]
fn a_satisfied_review_rule_is_not_named_missing() {
    let probes = FakeGh {
        ok: true,
        output: rules_output(json!([
            {"type": "pull_request", "parameters": {"required_approving_review_count": 2}},
        ])),
        review_decision: "APPROVED".to_string(),
        calls: RefCell::new(Vec::new()),
    };
    let out = merge_blocker(
        &probes,
        &json!({"merge_state": "blocked", "base_ref": "main", "cwd": "/repo", "pr": 7}),
    );
    assert_eq!(out["blockers"], json!(["github_blocked"]));
    // The review requirement is met; naming it would send a worker hunting
    // for an approval that exists. The read then explains nothing, so the
    // missing list is null, not an empty array.
    assert!(out["missing_required_checks"].is_null());
}

#[test]
fn the_steps_array_derives_the_failed_step_window() {
    // The Python transport passes the job's steps[] it already holds; the
    // failed step's [started_at, completed_at] scopes the block.
    let steps = json!([
        {"name": "cargo test --lib --bins", "conclusion": "success",
         "started_at": "2026-09-18T13:00:00Z", "completed_at": "2026-09-18T13:00:04Z"},
        {"name": "cargo test --test", "conclusion": "failure",
         "started_at": "2026-09-18T13:00:05Z", "completed_at": "2026-09-18T13:00:06Z"},
    ]);
    let out = failure_cause(&json!({"log": cross_door_log(), "window": steps}));
    assert_eq!(out["source"], "cargo");
    assert!(out["cause"]
        .as_str()
        .unwrap()
        .contains("cross_door_property.rs:535"));
}

#[test]
fn unknown_ops_are_refused_by_name() {
    let out = run_op("status-nonsense", &json!({}));
    assert!(out.contains("unknown op status-nonsense"));
}

// --- zero-job runs: a run that failed before minting a job ---------------

/// The 504b255 specimen: two failed cli-ci runs with 0 jobs (one newest),
/// one successful rust-ci run a check run links to.
fn specimen_runs() -> Vec<Value> {
    json!([
        {"id": 35344487208u64, "path": ".github/workflows/cli-ci.yml", "status": "completed",
         "conclusion": "failure", "created_at": "2026-09-19T06:00:00Z",
         "html_url": "https://github.com/o/r/actions/runs/35344487208"},
        {"id": 35366958901u64, "path": ".github/workflows/cli-ci.yml", "status": "completed",
         "conclusion": "failure", "created_at": "2026-09-19T07:00:00Z",
         "html_url": "https://github.com/o/r/actions/runs/35366958901"},
        {"id": 35344488345u64, "path": ".github/workflows/rust-ci.yml", "status": "completed",
         "conclusion": "success", "created_at": "2026-09-19T06:00:00Z",
         "html_url": "https://github.com/o/r/actions/runs/35344488345"},
    ])
    .as_array()
    .unwrap()
    .clone()
}

fn specimen_check_runs() -> Vec<Value> {
    json!([
        {"name": "rust-ci", "status": "completed", "conclusion": "success",
         "details_url": "https://github.com/o/r/actions/runs/35344488345/job/99"},
    ])
    .as_array()
    .unwrap()
    .clone()
}

#[test]
fn ac1_hp_the_newest_unlinked_failed_run_is_named_by_path() {
    let zero = 0u64;
    let found = zero_job_failures(&specimen_runs(), &specimen_check_runs(), &|_| Ok(zero)).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].path, ".github/workflows/cli-ci.yml");
    assert_eq!(
        found[0].url,
        "https://github.com/o/r/actions/runs/35366958901"
    );
    assert_eq!(found[0].conclusion, "failure");
}

#[test]
fn ac1_err_a_failed_jobs_read_is_err_and_a_missing_runs_key_is_an_error() {
    assert!(zero_job_failures(
        &[json!({"id": 7u64, "path": "w.yml", "status": "completed", "conclusion": "failure"})],
        &[],
        &|_| Err("jobs read failed".to_string()),
    )
    .is_err());
    // The op answers an error for a malformed payload, never empty rows.
    let out = run_op(
        "status-zero-job-runs",
        &json!({"slug": "o/r", "check_runs": []}),
    );
    assert!(out.contains("\"error\""), "got {out}");
    assert!(!out.contains("\"rows\""));
}

#[test]
fn ac1_edge_a_linked_run_never_calls_jobs_total() {
    let runs = json!([
        {"id": 9u64, "path": "w.yml", "status": "completed", "conclusion": "failure",
         "html_url": "https://github.com/o/r/actions/runs/9"},
    ])
    .as_array()
    .unwrap()
    .clone();
    let checks = json!([
        {"name": "w", "details_url": "https://github.com/o/r/actions/runs/9/job/3"},
    ])
    .as_array()
    .unwrap()
    .clone();
    let found = zero_job_failures(&runs, &checks, &|_| panic!("jobs_total must not run")).unwrap();
    assert!(found.is_empty());
}

#[test]
fn ac1_edge_a_newer_success_supersedes_an_older_zero_job_failure() {
    let runs = json!([
        {"id": 5u64, "path": "w.yml", "status": "completed", "conclusion": "failure",
         "html_url": "https://github.com/o/r/actions/runs/5"},
        {"id": 6u64, "path": "w.yml", "status": "completed", "conclusion": "success",
         "html_url": "https://github.com/o/r/actions/runs/6"},
    ])
    .as_array()
    .unwrap()
    .clone();
    let found = zero_job_failures(&runs, &[], &|_| panic!("jobs_total must not run")).unwrap();
    assert!(found.is_empty());
}

#[test]
fn ac1_edge_a_failed_run_with_jobs_is_not_reported() {
    let runs = json!([
        {"id": 7u64, "path": "w.yml", "status": "completed", "conclusion": "failure",
         "html_url": "https://github.com/o/r/actions/runs/7"},
    ])
    .as_array()
    .unwrap()
    .clone();
    let found = zero_job_failures(&runs, &[], &|_| Ok(3)).unwrap();
    assert!(found.is_empty());
}

#[test]
fn the_op_paginates_the_runs_listing_when_the_payload_names_a_sha() {
    let probes = FakeGh {
        ok: true,
        // `--paginate --slurp` output: a JSON array of pages. The zero-job
        // failure lives on page 2.
        output: r#"[{"workflow_runs":[{"id":1,"path":"w.yml","status":"completed","conclusion":"success","html_url":"https://github.com/o/r/actions/runs/1"}]},{"workflow_runs":[{"id":2,"path":"w.yml","status":"completed","conclusion":"failure","created_at":"2026-09-19T07:00:00Z","html_url":"https://github.com/o/r/actions/runs/2"}]}]"#
            .to_string(),
        review_decision: String::new(),
        calls: RefCell::new(Vec::new()),
    };
    let payload = json!({"slug": "o/r", "cwd": "/repo", "sha": "abc123", "check_runs": []});
    let out: Value = serde_json::from_str(&zero_job_runs_op(&probes, &payload).to_string())
        .unwrap_or(Value::Null);
    let rows = out["rows"].as_array().expect("rows array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], "w.yml");
    let calls = probes.calls.borrow();
    assert!(calls[0]
        .iter()
        .any(|a| a.contains("actions/runs?head_sha=abc123")));
    assert!(calls[0].iter().any(|a| a == "--paginate"));
}

#[test]
fn the_op_answers_rows_in_the_python_rollup_shape() {
    let probes = FakeGh {
        ok: true,
        output: r#"{"total_count": 0}"#.to_string(),
        review_decision: String::new(),
        calls: RefCell::new(Vec::new()),
    };
    let payload = json!({
        "slug": "o/r",
        "cwd": "/repo",
        "runs": specimen_runs(),
        "check_runs": specimen_check_runs(),
    });
    let out: Value = serde_json::from_str(&zero_job_runs_op(&probes, &payload).to_string())
        .unwrap_or(Value::Null);
    let rows = out["rows"].as_array().expect("rows array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], ".github/workflows/cli-ci.yml");
    assert_eq!(rows[0]["status"], "completed");
    assert_eq!(rows[0]["conclusion"], "failure");
    assert_eq!(rows[0]["workflow"], ".github/workflows/cli-ci.yml");
    assert_eq!(
        rows[0]["detailsUrl"],
        "https://github.com/o/r/actions/runs/35366958901"
    );
    let calls = probes.calls.borrow();
    assert!(calls[0]
        .iter()
        .any(|a| a.contains("repos/o/r/actions/runs/35366958901/jobs")));
}
