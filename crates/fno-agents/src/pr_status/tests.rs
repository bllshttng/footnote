use super::*;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pr_status")
}

fn load_fixture(name: &str) -> Value {
    let text = std::fs::read_to_string(fixtures_dir().join(format!("{name}.json")))
        .unwrap_or_else(|e| panic!("fixture {name}: {e}"));
    serde_json::from_str(&text).unwrap()
}

/// Serves the fixture's raw gh responses off the same URL shapes the fake
/// runner matched on the Python side.
struct FakeGh {
    raw: Value,
    log_reads: AtomicUsize,
    /// Force the pulls read to fail with this stderr (refusal scenarios).
    pulls_fail: Option<String>,
}

impl FakeGh {
    fn from_fixture(name: &str) -> Self {
        let fixture = load_fixture(name);
        FakeGh {
            raw: fixture["inputs"]["raw"].clone(),
            log_reads: AtomicUsize::new(0),
            pulls_fail: None,
        }
    }
}

impl GhProbe for FakeGh {
    fn run_gh(&self, _cwd: &Path, args: &[String]) -> Result<(bool, String, String), String> {
        let cmd = args.join(" ");
        let serve = |v: &Value| (true, v.to_string(), String::new());
        if let Some(stderr) = &self.pulls_fail {
            if cmd.contains("/pulls/") {
                return Ok((false, String::new(), stderr.clone()));
            }
        }
        if cmd.contains("/pulls/") {
            return Ok(serve(&self.raw["pulls"]));
        }
        if cmd.contains("/check-runs") && cmd.contains("page=") {
            let page: usize = cmd
                .rsplit("page=")
                .next()
                .unwrap_or("1")
                .trim()
                .parse()
                .unwrap_or(1);
            let pages = self.raw["check_runs_pages"].as_array().unwrap();
            let body = pages
                .get(page - 1)
                .cloned()
                .unwrap_or(serde_json::json!({"total_count": 0, "check_runs": []}));
            return Ok(serve(&body));
        }
        if cmd.contains("/actions/runs?head_sha=") {
            return Ok(serve(&self.raw["runs_listing"]));
        }
        if cmd.contains("/actions/runs/") && cmd.contains("/jobs?per_page=") {
            // A run is zero-job in the fixture iff a zero_rows entry names
            // its run id in the details URL; every other run has jobs.
            let run_id = cmd
                .split("/actions/runs/")
                .nth(1)
                .unwrap_or("")
                .split('/')
                .next()
                .unwrap_or("");
            let zero = self.raw["zero_rows"]
                .as_array()
                .map(|rows| {
                    rows.iter().any(|r| {
                        r.get("detailsUrl")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .contains(&format!("/runs/{run_id}"))
                    })
                })
                .unwrap_or(false);
            let total = if zero { 0 } else { 2 };
            return Ok(serve(
                &serde_json::json!({"total_count": total, "jobs": []}),
            ));
        }
        if cmd.ends_with("/status") {
            return Ok(serve(&self.raw["statuses"]));
        }
        if cmd.contains("/actions/jobs/") && cmd.ends_with("/logs") {
            self.log_reads.fetch_add(1, Ordering::SeqCst);
            let job = cmd
                .rsplit("/jobs/")
                .next()
                .unwrap()
                .split('/')
                .next()
                .unwrap()
                .to_string();
            let failed = self.raw["failed_job_logs"]
                .get(&job)
                .and_then(Value::as_str);
            if let Some(stderr) = failed {
                return Ok((false, String::new(), stderr.to_string()));
            }
            let log = self.raw["job_logs"]
                .get(&job)
                .and_then(Value::as_str)
                .unwrap_or("");
            return Ok((true, log.to_string(), String::new()));
        }
        if cmd.contains("/actions/jobs/") {
            let job = cmd
                .rsplit("/jobs/")
                .next()
                .unwrap()
                .split('?')
                .next()
                .unwrap()
                .to_string();
            let steps = self.raw["job_steps"]
                .get(&job)
                .cloned()
                .unwrap_or(serde_json::json!([]));
            return Ok(serve(&serde_json::json!({ "steps": steps })));
        }
        if cmd.contains("rate_limit") {
            return Ok(serve(
                &serde_json::json!({ "resources": { "core": { "remaining": 10 } } }),
            ));
        }
        Err(format!("fixture fake has no answer for: {cmd}"))
    }
}

fn known_from_prior(_prior: &Value) -> BTreeMap<String, Value> {
    BTreeMap::new()
}

/// The `status-ci` row mapping: bucket intercepts (cancel, skipping), the
/// coverage projection dropped only when asked, statuses carried as rows.
#[test]
fn status_ci_maps_the_rollup_rows() {
    struct CiFake;
    impl GhProbe for CiFake {
        fn run_gh(&self, _cwd: &Path, args: &[String]) -> Result<(bool, String, String), String> {
            let cmd = args.join(" ");
            let serve = |v: &Value| Ok((true, v.to_string(), String::new()));
            if cmd.contains("/pulls/") {
                return serve(&json!({"head": {"sha": "abc123"}, "state": "open"}));
            }
            if cmd.contains("/check-runs") {
                return serve(&json!({
                    "total_count": 3,
                    "check_runs": [
                        {"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS",
                         "started_at": "2026-08-18T00:00:00Z", "details_url": "https://x/runs/1"},
                        {"name": "flaky", "status": "COMPLETED", "conclusion": "CANCELLED",
                         "started_at": "2026-08-18T00:00:01Z", "details_url": "https://x/runs/1"},
                        {"name": "docs", "status": "COMPLETED", "conclusion": "SKIPPED",
                         "started_at": "2026-08-18T00:00:02Z", "details_url": "https://x/runs/1"}
                    ]
                }));
            }
            if cmd.contains("/actions/runs?head_sha=") {
                return serve(&json!([]));
            }
            if cmd.contains("/actions/runs/") {
                return serve(&json!({"total_count": 0, "jobs": []}));
            }
            if cmd.ends_with("/status") {
                return serve(&json!({"statuses": [
                    {"context": "fno/review-coverage", "state": "FAILURE",
                     "created_at": "2026-08-18T00:00:03Z", "target_url": ""},
                    {"context": "deploy", "state": "success",
                     "created_at": "2026-08-18T00:00:04Z", "target_url": ""}
                ]}));
            }
            if cmd.contains("rate_limit") {
                return serve(&json!({"resources": {"core": {"remaining": 10}}}));
            }
            Err(format!("CiFake has no answer for: {cmd}"))
        }
    }
    let cwd = Path::new("/tmp");
    let rows = status_ci_rows(&CiFake, cwd, "Owner/Repo", 42, false).unwrap();
    assert_eq!(rows.len(), 5, "3 check runs + 2 statuses");
    assert_eq!(rows[0]["bucket"], json!("pass"));
    assert_eq!(
        rows[1]["bucket"],
        json!("cancel"),
        "CANCELLED intercepts classify"
    );
    assert_eq!(
        rows[2]["bucket"],
        json!("skipping"),
        "SKIPPED intercepts classify"
    );
    assert_eq!(rows[2]["state"], json!("SKIPPED"));
    assert_eq!(rows[4]["name"], json!("deploy"));
    assert_eq!(
        rows[4]["bucket"],
        json!("pass"),
        "a success status is a row too"
    );

    let rows = status_ci_rows(&CiFake, cwd, "Owner/Repo", 42, true).unwrap();
    assert_eq!(rows.len(), 4, "the coverage projection is dropped");
    assert!(
        rows.iter()
            .all(|r| r["name"] != json!("fno/review-coverage")),
        "{rows:?}"
    );
}

/// The assembled pr_json the fixture's raw responses produce: the same
/// construction the Python leg's fake runner drove at capture time.
fn pr_json_from(name: &str) -> Value {
    let fake = FakeGh::from_fixture(name);
    read_pr(&fake, Path::new("/tmp"), "Owner/Repo", 42).unwrap()
}

/// The six live-read fixtures replay through read_pr + verdict_for and every
/// count, the head, the PR state, and the mergeable word equal the golden.
#[test]
fn read_fixtures_replay_to_the_golden_verdict() {
    for name in [
        "green_settled",
        "red_detailed_capped",
        "pending_mixed",
        "all_status_contexts",
        "zero_job_runs",
        "terminal_merged",
    ] {
        let fixture = load_fixture(name);
        let fake = FakeGh::from_fixture(name);
        let pr_json = read_pr(&fake, Path::new("/tmp"), "Owner/Repo", 42)
            .unwrap_or_else(|e| panic!("{name}: read failed: {}", e.text));
        let expected: Value =
            serde_json::from_str(fixture["expected"]["stdout"].as_str().unwrap()).unwrap();
        let rollup =
            without_coverage_statuses(pr_json["statusCheckRollup"].as_array().expect("rollup"));
        let (verdict, code, counts) = verdict_for(&rollup);
        assert_eq!(
            Value::String(verdict.clone()),
            expected["verdict"],
            "{name}: verdict"
        );
        assert_eq!(code, expected_repr_code(&fixture), "{name}: exit code");
        assert_eq!(counts, expected["checks"], "{name}: checks counts");
        assert_eq!(pr_json["headRefOid"], expected["head"], "{name}: head");
        assert_eq!(pr_json["state"], expected["pr_state"], "{name}: pr_state");
        assert_eq!(
            pr_json["mergeable"], expected["mergeable"],
            "{name}: mergeable"
        );
    }
}

fn expected_repr_code(fixture: &Value) -> i32 {
    fixture["expected"]["exit"].as_i64().unwrap_or(-1) as i32
}

/// The red fixture's failure detail replays from the canned job logs: five
/// detailed entries plus the truncation entry, equal to the golden.
#[test]
fn red_fixture_collects_the_golden_failures() {
    let fake = FakeGh::from_fixture("red_detailed_capped");
    let pr_json = read_pr(&fake, Path::new("/tmp"), "Owner/Repo", 42).unwrap();
    let rollup = without_coverage_statuses(pr_json["statusCheckRollup"].as_array().unwrap());
    let failing: Vec<Value> = crate::check_supersession::latest_per_name(&Value::Array(rollup))
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| classify_check(c) == "fail" && has_settled_marker(c))
        .cloned()
        .collect();
    let failures = collect_failures(
        &fake,
        Path::new("/tmp"),
        "Owner--Repo",
        &failing,
        &known_from_prior(&Value::Null),
    );
    let expected: Value = serde_json::from_str(
        load_fixture("red_detailed_capped")["expected"]["stdout"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        Value::Array(failures),
        expected["failures"],
        "failure detail"
    );
}

/// The refusal fixture's stderr classifies as a secondary rate limit, and a
/// refused read never returns a partial rollup.
#[test]
fn refusal_reads_the_rate_limit_class() {
    let fixture = load_fixture("rest_refusal_rate_limit");
    let stderr = fixture["inputs"]["fetch_stderr"].as_str().unwrap();
    let mut fake = FakeGh::from_fixture("rest_refusal_rate_limit");
    fake.pulls_fail = Some(stderr.to_string());
    let err = read_pr(&fake, Path::new("/tmp"), "Owner/Repo", 42).unwrap_err();
    assert_eq!(err.rate_limit_class, "secondary", "rate limit class");
    assert!(
        err.text.contains(stderr),
        "the refusal names the matched line"
    );
}

/// A prior row at the same head replays entries by job id with zero log
/// reads (the prior_row_replays_failures fixture's whole point).
#[test]
fn prior_row_replays_failures_by_job_id_with_no_log_reads() {
    let fixture = load_fixture("prior_row_replays_failures");
    let prior = fixture["inputs"]["cache"]["seed_rows"][format!(
        "Owner--Repo-42-{}",
        &"b41ac4bfeedface0123456789abcdef012345678"[..12]
    )]["output"]
        .clone();
    let mut known = BTreeMap::new();
    for f in prior["failures"].as_array().unwrap() {
        let id = f["job_id"].as_str().unwrap().to_string();
        known.insert(id, f.clone());
    }
    let fake = FakeGh::from_fixture("prior_row_replays_failures");
    let pr_json = read_pr(&fake, Path::new("/tmp"), "Owner/Repo", 42).unwrap();
    let rollup = without_coverage_statuses(pr_json["statusCheckRollup"].as_array().unwrap());
    let failing: Vec<Value> = crate::check_supersession::latest_per_name(&Value::Array(rollup))
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| classify_check(c) == "fail" && has_settled_marker(c))
        .cloned()
        .collect();
    let failures = collect_failures(&fake, Path::new("/tmp"), "Owner--Repo", &failing, &known);
    assert_eq!(
        Value::Array(failures),
        prior["failures"],
        "replayed entries"
    );
    assert_eq!(fake.log_reads.load(Ordering::SeqCst), 0, "zero log reads");
}

/// A log read lands in the job cache and the second read never spends a gh
/// call; a failed log read is never cached. One test, one tempdir: the env
/// override is process-global, and cargo runs tests in parallel threads.
#[test]
fn job_log_caches_one_attempt_and_never_a_failure() {
    let _guard = super::cache_env_lock();
    let green = FakeGh::from_fixture("green_settled");
    let red = FakeGh::from_fixture("red_detailed_capped");
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("FNO_PR_STATUS_CACHE_DIR", dir.path());
    let cwd = Path::new("/tmp");
    let first = job_log(&green, cwd, "Owner--Repo", "Owner", "Repo", "1001").unwrap();
    let second = job_log(&green, cwd, "Owner--Repo", "Owner", "Repo", "1001").unwrap();
    assert_eq!(first, second, "the same log both times");
    assert_eq!(
        green.log_reads.load(Ordering::SeqCst),
        1,
        "the second read serves from the cache"
    );
    let cached = std::fs::read_to_string(dir.path().join("job-Owner--Repo-1001.json")).unwrap();
    let row: Value = serde_json::from_str(&cached).unwrap();
    assert!(row["ts"].as_f64().is_some(), "the row carries ts");
    assert_eq!(row["log"].as_str().unwrap(), first);

    let err = job_log(&red, cwd, "Owner--Repo", "Owner", "Repo", "2003")
        .err()
        .expect("a failed read is an Err");
    assert!(err.contains("secondary rate limit"), "{err}");
    assert!(
        !dir.path().join("job-Owner--Repo-2003.json").exists(),
        "a failed read writes no cache row"
    );
}

/// The settled-marker rule: a cancelled run is red AND unsettled; an
/// in-progress run is pending, never red.
#[test]
fn cancelled_reads_red_and_unsettled() {
    let fake = FakeGh::from_fixture("pending_mixed");
    let pr_json = read_pr(&fake, Path::new("/tmp"), "Owner/Repo", 42).unwrap();
    let rollup = without_coverage_statuses(pr_json["statusCheckRollup"].as_array().unwrap());
    let (verdict, code, counts) = verdict_for(&rollup);
    assert_eq!(verdict, "red");
    assert_eq!(code, 1);
    assert_eq!(counts["unsettled"], json!(2), "both runs are unsettled");
    assert_eq!(
        counts["fail"],
        json!(1),
        "only the cancelled run counts fail"
    );
}

/// The composer replays the live-read fixtures: the payload it assembles and
/// the stderr lines it prints equal the golden, field for field and line for
/// line. Failure detail arrives resolved (its own collection is pinned by the
/// red fixture's collection test).
#[test]
fn composer_replays_the_goldens() {
    for name in [
        "green_settled",
        "red_detailed_capped",
        "pending_mixed",
        "all_status_contexts",
        "zero_job_runs",
        "terminal_merged",
    ] {
        let fixture = load_fixture(name);
        let expected: Value =
            serde_json::from_str(fixture["expected"]["stdout"].as_str().unwrap()).unwrap();
        let inputs = crate::pr_status::compose::ComposeInputs {
            pr: "42".to_string(),
            pr_json: pr_json_from(name),
            rerun_recovery: fixture["inputs"]["rerun_recovery"].clone(),
            branch_history: fixture["inputs"]["branch_history"].clone(),
            optional_reviews: fixture["inputs"]["optional_reviews"].clone(),
            coverage_row: fixture["inputs"]["coverage_row"].clone(),
            hold_reason: fixture["inputs"]["hold_reason"].clone(),
            review_activity: fixture["inputs"]["review_activity"].clone(),
            receipt: fixture["inputs"]["receipt"].clone(),
            github_merge_blockers: fixture["inputs"]["github_merge_blockers"].clone(),
            merge_authority: fixture["inputs"]["merge_authority"].clone(),
            merge_execution: fixture["inputs"]["merge_execution"].clone(),
            failures: expected.get("failures").cloned().unwrap_or(Value::Null),
            review_lane: fixture["inputs"]["review_lane"].as_bool().unwrap_or(false),
        };
        let (code, payload, stderr) = crate::pr_status::compose::compose_payload(&inputs);
        assert_eq!(
            code,
            fixture["expected"]["exit"].as_i64().unwrap() as i32,
            "{name}: exit"
        );
        assert_eq!(payload, expected, "{name}: payload");
        let expected_lines: Vec<String> = fixture["expected"]["stderr_lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(stderr, expected_lines, "{name}: stderr");
    }
}

/// The refused read composes the error payload: exit 4, the reason field,
/// the rate-limit class, and the one stderr line.
#[test]
fn composer_replays_the_refusal() {
    let fixture = load_fixture("rest_refusal_rate_limit");
    let reason = RestReason {
        text: fixture["inputs"]["fetch_stderr"]
            .as_str()
            .unwrap()
            .to_string(),
        rate_limit_class: fixture["inputs"]["rate_limit_class"]
            .as_str()
            .unwrap()
            .to_string(),
    };
    let (code, payload, stderr) = crate::pr_status::compose::error_payload("42", &reason);
    assert_eq!(code, 4);
    let expected: Value =
        serde_json::from_str(fixture["expected"]["stdout"].as_str().unwrap()).unwrap();
    assert_eq!(payload, expected, "error payload");
    let expected_lines: Vec<String> = fixture["expected"]["stderr_lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(stderr, expected_lines, "error stderr");
}
