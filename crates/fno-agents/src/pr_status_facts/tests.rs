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

struct AnnotationGh {
    ok: bool,
    annotations: String,
    calls: RefCell<Vec<Vec<String>>>,
}

impl GhProbe for AnnotationGh {
    fn run_gh(&self, _cwd: &Path, args: &[String]) -> Result<(bool, String, String), String> {
        self.calls.borrow_mut().push(args.to_vec());
        let path = args.join(" ");
        if path.contains("actions/runs?head_sha=") {
            return Ok((true, r#"[{"workflow_runs":[]}]"#.to_string(), String::new()));
        }
        if path.contains("/check-runs/123/annotations") {
            return Ok((self.ok, self.annotations.clone(), String::new()));
        }
        if path.contains("jobs?per_page=1") {
            return Ok((true, r#"{"total_count":0}"#.to_string(), String::new()));
        }
        Ok((true, String::new(), String::new()))
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
fn block_rows() {
    let probes = FakeGh {
        ok: true,
        output: String::new(),
        review_decision: String::new(),
        calls: RefCell::new(Vec::new()),
    };
    let out = merge_blocker(
        &probes,
        &json!({"merge_state": "clean", "mergeable": "CONFLICTING", "cwd": "/repo"}),
    );
    assert_eq!(out["blockers"], json!(["not_mergeable_conflicting"]));

    // Still computing: fail closed, exactly like the pre-port conjunction.
    let out = merge_blocker(
        &probes,
        &json!({"merge_state": "clean", "mergeable": null, "cwd": "/repo"}),
    );
    assert_eq!(out["blockers"], json!(["not_mergeable_unknown"]));

    // The one positive answer adds nothing; an absent key adds nothing.
    let out = merge_blocker(
        &probes,
        &json!({"merge_state": "clean", "mergeable": "MERGEABLE", "cwd": "/repo"}),
    );
    assert!(out["blockers"].as_array().unwrap().is_empty());
    let out = merge_blocker(&probes, &json!({"merge_state": "clean", "cwd": "/repo"}));
    assert!(out["blockers"].as_array().unwrap().is_empty());

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
fn cause_rows() {
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

    let log = [
        "pre-group error: from an earlier step",
        "##[group]Sync + build",
        "in-step error: the real one",
        "step failed, stopping (fail-fast): Sync + build",
    ]
    .join("\n");
    let out = failure_cause(&json!({"log": log, "step": "Sync + build"}));
    assert_eq!(out["cause"], json!("in-step error: the real one"));

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

    let long = format!(
        "test big ... FAILED\nthread 'big' panicked at a.rs:1:1:\n{}",
        "x".repeat(600)
    );
    let out = failure_cause(&json!({"log": long}));
    let cause = out["cause"].as_str().unwrap();
    assert_eq!(out["truncated"], true);
    assert!(cause.ends_with(" chars cut]"));
    assert!(cause.chars().count() < 600);

    let out = failure_cause(&json!({"log": ""}));
    assert!(out["cause"].is_null());
    assert_eq!(out["source"], "tail");

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

    let log = "boom error: x\nstep failed, stopping fail-fast: s\n".to_string();
    let out = failure_cause(&json!({"log": log, "step": "s"}));
    assert_eq!(out["cause"], json!("boom error: x"));
}

#[test]
fn rule_rows() {
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

    let steps = json!([
        {"name": "check-file-budget", "conclusion": "failure",
         "started_at": "2026-09-18T00:54:28Z", "completed_at": "2026-09-18T00:54:29Z"},
        {"name": "Both shards passed", "conclusion": "success",
         "started_at": "2026-09-18T00:54:29Z", "completed_at": "2026-09-18T00:54:31Z"},
    ]);
    let log = [
        "2026-09-18T00:54:29Z check-file-budget: cli/src/fno added +346 lines",
        "2026-09-18T00:54:30.9958Z 20 passed, 0 failed",
    ]
    .join("\n");
    let out = failure_cause(&json!({"log": log, "window": steps}));
    assert!(out["cause"].as_str().unwrap().contains("check-file-budget"));
    assert!(!out["cause"].as_str().unwrap().contains("20 passed"));

    let log = [
        "test-agents=failure test-mux=success",
        "##[error]Process completed with exit code 1.",
        "Cleaning up orphan processes",
    ]
    .join("\n");
    let out = failure_cause(&json!({"log": log}));
    assert_eq!(out["cause"], "test-agents=failure test-mux=success");

    let log = [
        "the policy check found the defect",
        "##[group]Post job cleanup",
        "##[endgroup]",
        "##[warning]Node.js 20 is deprecated",
    ]
    .join("\n");
    let out = failure_cause(&json!({"log": log}));
    assert_eq!(out["cause"], "the policy check found the defect");
}

#[test]
fn op_rows() {
    let out = run_op("status-nonsense", &json!({}));
    assert!(out.contains("unknown op status-nonsense"));

    let zero = 0u64;
    let found = zero_job_failures(&specimen_runs(), &specimen_check_runs(), &|_| Ok(zero)).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].path, ".github/workflows/cli-ci.yml");
    assert_eq!(
        found[0].url,
        "https://github.com/o/r/actions/runs/35366958901"
    );
    assert_eq!(found[0].conclusion, "failure");

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

    let runs = json!([
        {"id": 7u64, "path": "w.yml", "status": "completed", "conclusion": "failure",
         "html_url": "https://github.com/o/r/actions/runs/7"},
    ])
    .as_array()
    .unwrap()
    .clone();
    let found = zero_job_failures(&runs, &[], &|_| Ok(3)).unwrap();
    assert!(found.is_empty());

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
    // The listing rides the answer for the caller's workflow-name mapping.
    assert_eq!(out["listing"].as_array().map(|a| a.len()), Some(2));
    let calls = probes.calls.borrow();
    assert!(calls[0]
        .iter()
        .any(|a| a.contains("actions/runs?head_sha=abc123")));
    assert!(calls[0].iter().any(|a| a == "--paginate"));

    let probes = FakeGh {
        ok: true,
        // `--paginate --slurp` output: one page holding the specimen runs.
        output: format!(
            r#"[{{"workflow_runs":{}}}] "#,
            serde_json::to_string(&specimen_runs()).unwrap()
        ),
        review_decision: String::new(),
        calls: RefCell::new(Vec::new()),
    };
    let payload = json!({
        "slug": "o/r",
        "cwd": "/repo",
        "sha": "abc123",
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
    assert!(calls.iter().any(|argv| argv
        .iter()
        .any(|a| a.contains("repos/o/r/actions/runs/35366958901/jobs"))));
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
fn annot_rows() {
    let annotations = json!([
        {"annotation_level": "notice", "message": "some notice"},
        {"annotation_level": "failure", "message": "The job has exceeded the maximum execution time of 35m0s"},
        {"annotation_level": "failure", "message": "later timeout"},
    ]);
    assert_eq!(
        timeout_annotation(&annotations),
        Some("The job has exceeded the maximum execution time of 35m0s".to_string())
    );

    let message = "The job has exceeded the maximum execution time of 35m0s";
    let probes = AnnotationGh {
        ok: true,
        annotations: json!([{"annotation_level": "failure", "message": message}]).to_string(),
        calls: RefCell::new(Vec::new()),
    };
    let payload = json!({
        "slug": "o/r", "cwd": "/repo", "sha": "abc123",
        "check_runs": [{"id": 123, "name": "stress", "status": "completed",
            "conclusion": "cancelled", "output": {"annotations_count": 1}}],
    });
    let out = zero_job_runs_op(&probes, &payload);
    assert!(out.get("error").is_none(), "{out}");
    assert_eq!(out["check_runs"][0]["conclusion"], "timed_out");
    assert_eq!(out["check_runs"][0]["timeout"], message);
    assert!(probes.calls.borrow().iter().any(|args| args
        .iter()
        .any(|arg| arg.contains("repos/o/r/check-runs/123/annotations"))));

    for (ok, annotations) in [
        (false, "annotation read failed"),
        (
            true,
            r#"[{"annotation_level":"failure","message":"Canceling since a higher priority waiting request exists"}]"#,
        ),
    ] {
        let probes = AnnotationGh {
            ok,
            annotations: annotations.to_string(),
            calls: RefCell::new(Vec::new()),
        };
        let payload = json!({
            "slug": "o/r", "cwd": "/repo", "sha": "abc123",
            "check_runs": [{"id": 123, "name": "stress", "status": "completed",
                "conclusion": "cancelled", "output": {"annotations_count": 1}}],
        });
        let out = zero_job_runs_op(&probes, &payload);
        assert!(out.get("error").is_none(), "{out}");
        assert_eq!(out["check_runs"][0]["conclusion"], "cancelled");
        assert!(out["check_runs"][0].get("timeout").is_none());
    }
}

#[test]
fn rest_get_op_reads_eligible_paths_and_leaves_the_rest_to_the_caller() {
    let probes = FakeGh {
        ok: true,
        output: r#"{"state":"open"}"#.to_string(),
        review_decision: String::new(),
        calls: RefCell::new(Vec::new()),
    };
    let admit = |_: &[String]| None;
    let out = rest_get_op(
        &probes,
        admit,
        &json!({"cwd": "/repo", "path": "repos/o/r/pulls/1"}),
    );
    assert_eq!(out["eligible"], true);
    assert_eq!(out["ok"], true);
    assert_eq!(out["stdout"], r#"{"state":"open"}"#);
    assert_eq!(probes.calls.borrow().len(), 1);

    for path in ["rate_limit", "repos/o/r/pulls?per_page=100&page=2"] {
        let out = rest_get_op(&probes, admit, &json!({"cwd": "/repo", "path": path}));
        assert_eq!(out["eligible"], false, "{path}");
    }
    assert_eq!(
        probes.calls.borrow().len(),
        1,
        "an ineligible path spawns nothing"
    );

    let hold = |_: &[String]| Some("gh budget: held by the test".to_string());
    let out = rest_get_op(
        &probes,
        hold,
        &json!({"cwd": "/repo", "path": "repos/o/r/pulls/1"}),
    );
    assert_eq!(out["ok"], false);
    assert!(out["stderr"].as_str().unwrap().contains("gh budget:"));
    assert_eq!(probes.calls.borrow().len(), 1, "a held read spawns nothing");
}

// --- status-pr-activity ------------------------------------------------------

/// A path-keyed fake: the first matching needle answers; anything else is an
/// empty JSON array, so an unplanned read reads "no more pages".
struct ActivityGh {
    answers: Vec<(&'static str, bool, String, String)>,
    calls: RefCell<Vec<String>>,
}

impl GhProbe for ActivityGh {
    fn run_gh(&self, _cwd: &Path, args: &[String]) -> Result<(bool, String, String), String> {
        let cmd = args.join(" ");
        self.calls.borrow_mut().push(cmd.clone());
        for (needle, ok, body, stderr) in &self.answers {
            if cmd.contains(needle) {
                return Ok((*ok, body.clone(), stderr.clone()));
            }
        }
        Ok((true, "[]".to_string(), String::new()))
    }
}

#[test]
fn pr_activity_takes_newest_matched_review_or_comment() {
    let probes = ActivityGh {
        answers: vec![
            (
                "/pulls/7/reviews",
                true,
                json!([
                    {"user": {"login": "codex-reviewer[bot]"},
                     "submitted_at": "2026-10-15T09:00:00Z", "state": "COMMENTED"},
                    {"user": {"login": "unconfigured"}, "submitted_at": "2026-10-16T09:00:00Z"},
                    {"user": {"login": "pending-bot"}, "state": "PENDING"}
                ])
                .to_string(),
                String::new(),
            ),
            (
                "/pulls/7",
                true,
                json!({"created_at": "2026-10-01T10:00:00Z", "draft": false}).to_string(),
                String::new(),
            ),
            (
                "/issues/7/comments",
                true,
                json!([
                    {"user": {"login": "codex-reviewer"}, "created_at": "2026-10-16T12:00:00Z"},
                    {"user": {"login": "unconfigured"}, "created_at": "2026-10-16T13:00:00Z"}
                ])
                .to_string(),
                String::new(),
            ),
        ],
        calls: RefCell::new(Vec::new()),
    };
    let out = pr_activity_op(
        &probes,
        &json!({"cwd": "/repo", "slug": "o/r", "pr": 7, "reviewers": ["codex"]}),
    );
    assert_eq!(out["opened_at"], json!("2026-10-01T10:00:00Z"));
    assert_eq!(out["is_draft"], json!(false));
    assert_eq!(
        out["latest_review_ts"],
        json!("2026-10-16T12:00:00Z"),
        "the newest MATCHED row wins; the unconfigured 13:00 comment does not"
    );
    let calls = probes.calls.borrow();
    assert_eq!(
        calls.len(),
        3,
        "one pulls + one reviews page + one comments page"
    );
    assert!(
        calls
            .iter()
            .any(|c| c.contains("/pulls/7/reviews?per_page=100&page=1")),
        "{calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|c| c.contains("/issues/7/comments?per_page=100&page=1")),
        "{calls:?}"
    );
}

#[test]
fn pr_activity_failed_read_answers_error() {
    let probes = ActivityGh {
        answers: vec![(
            "/issues/7/comments",
            false,
            String::new(),
            "HTTP 404: Not Found\n(second line)".to_string(),
        )],
        calls: RefCell::new(Vec::new()),
    };
    let out = pr_activity_op(
        &probes,
        &json!({"cwd": "/repo", "slug": "o/r", "pr": 7, "reviewers": ["codex"]}),
    );
    assert!(out.get("error").is_some(), "{out}");
    assert!(
        out["error"].as_str().unwrap().contains(
            "repos/o/r/issues/7/comments?per_page=100&page=1 failed: HTTP 404: Not Found"
        ),
        "{out}"
    );
    assert!(
        out.get("latest_review_ts").is_none(),
        "an error carries no activity fields: {out}"
    );
}

#[test]
fn pr_activity_empty_reviewers_skip_the_activity_reads() {
    let probes = ActivityGh {
        answers: vec![(
            "/pulls/7",
            true,
            json!({"created_at": "2026-10-01T10:00:00Z", "draft": true}).to_string(),
            String::new(),
        )],
        calls: RefCell::new(Vec::new()),
    };
    let out = pr_activity_op(
        &probes,
        &json!({"cwd": "/repo", "slug": "o/r", "pr": 7, "reviewers": []}),
    );
    assert_eq!(out["opened_at"], json!("2026-10-01T10:00:00Z"));
    assert_eq!(out["is_draft"], json!(true));
    assert_eq!(
        out["latest_review_ts"],
        Value::Null,
        "no configured reviewers: null without the reads"
    );
    let calls = probes.calls.borrow();
    assert_eq!(
        calls.len(),
        1,
        "the review and comment reads are skipped, not spent: {calls:?}"
    );
}
