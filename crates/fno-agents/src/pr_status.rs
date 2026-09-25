//! One PR-status owner: the REST reader, the verdict, the failure detail,
//! and the job-log cache that heal and the status verb share.
//!
//! Ported from the Python legs this module retires (`fno.pr._rest`
//! `fetch_pr_rest`, `fno.pr._status` `verdict_for`, `fno.pr._failures`
//! `collect_failures`); the port protocol and the golden fixtures it must
//! replay live in docs/architecture/dual-implementation-inventory.md and
//! crates/fno-agents/tests/fixtures/pr_status/. Every gh read rides the
//! `GhProbe` seam so tests run offline.

use crate::king_board::prs::classify_check;
use crate::pr_status_facts::{failure_cause, zero_job_scan, GhProbe};
use regex::Regex;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The rollup contexts that project review coverage, not generic CI; the
/// verdict drops them before classifying (`_status.without_coverage_statuses`).
pub(crate) const COVERAGE_STATUS_CONTEXTS: [&str; 2] =
    ["fno/review-coverage", "fno/review-coverage-unavailable"];

/// Conclusions that prove a result EXISTS: pass states plus fail states minus
/// the two taken-away conclusions (CANCELLED, STALE). A cancelled run is red
/// AND unsettled; the exit code is always the verdict's.
const SETTLED_STATES: [&str; 8] = [
    "SUCCESS",
    "NEUTRAL",
    "SKIPPED",
    "FAILURE",
    "TIMED_OUT",
    "ACTION_REQUIRED",
    "STARTUP_FAILURE",
    "ERROR",
];

/// Cap on failing checks detailed per status read; the truncation is itself
/// reported, never silent (`_failures.MAX_DETAILED_FAILURES`).
pub(crate) const MAX_DETAILED_FAILURES: usize = 5;

/// A failed REST read, carrying the machine-consumable rate-limit class the
/// cache arms its backoff on (`_rest.RestReason`).
#[derive(Debug, Clone)]
pub(crate) struct RestReason {
    pub text: String,
    pub rate_limit_class: String,
}

impl RestReason {
    fn new(text: impl Into<String>, rate_limit_class: &str) -> Self {
        RestReason {
            text: text.into(),
            rate_limit_class: rate_limit_class.to_string(),
        }
    }
}

fn s_str<'a>(check: &'a Value, key: &str) -> &'a str {
    check.get(key).and_then(Value::as_str).unwrap_or("")
}

fn alt_conclusion(check: &Value) -> String {
    let conclusion = s_str(check, "conclusion");
    if !conclusion.is_empty() {
        return conclusion.to_uppercase();
    }
    s_str(check, "state").to_uppercase()
}

/// A rollup entry's pass/fail/pending class, mirroring
/// `king_board::prs::classify_check` with the same case handling.
pub(crate) fn has_settled_marker(check: &Value) -> bool {
    let status = s_str(check, "status").to_uppercase();
    if !status.is_empty() && status != "COMPLETED" {
        return false;
    }
    SETTLED_STATES.contains(&alt_conclusion(check).as_str())
}

/// Drop the review-coverage projections before classifying generic CI.
pub(crate) fn without_coverage_statuses(rollup: &[Value]) -> Vec<Value> {
    rollup
        .iter()
        .filter(|check| {
            let context = s_str(check, "context");
            let name = s_str(check, "name");
            !COVERAGE_STATUS_CONTEXTS.contains(&context)
                && !COVERAGE_STATUS_CONTEXTS.contains(&name)
        })
        .cloned()
        .collect()
}

/// The pure verdict: (word, exit code, counts). Empty rollup reads unknown
/// and so does an all-StatusContext one - zero real check-runs never reads
/// green (docs/architecture/pr-status-verdict.md).
pub(crate) fn verdict_for(rollup: &[Value]) -> (String, i32, Value) {
    let deduped = crate::check_supersession::latest_per_name(&Value::Array(rollup.to_vec()));
    let deduped = deduped.as_array().cloned().unwrap_or_default();
    let mut counts = json!({
        "total": deduped.len(),
        "check_runs": 0,
        "statuses": 0,
        "fail_check_runs": 0,
        "fail_statuses": 0,
        "pass": 0,
        "fail": 0,
        "pending": 0,
        "unsettled": 0,
        "unsettled_fail": 0,
    });
    for c in &deduped {
        let kind = classify_check(c);
        counts[kind] = json!(counts[kind].as_i64().unwrap_or(0) + 1);
        if !has_settled_marker(c) {
            counts["unsettled"] = json!(counts["unsettled"].as_i64().unwrap_or(0) + 1);
            if kind == "fail" {
                counts["unsettled_fail"] =
                    json!(counts["unsettled_fail"].as_i64().unwrap_or(0) + 1);
            }
        }
        if !s_str(c, "name").is_empty() {
            counts["check_runs"] = json!(counts["check_runs"].as_i64().unwrap_or(0) + 1);
            if kind == "fail" {
                counts["fail_check_runs"] =
                    json!(counts["fail_check_runs"].as_i64().unwrap_or(0) + 1);
            }
        } else if !s_str(c, "context").is_empty() {
            counts["statuses"] = json!(counts["statuses"].as_i64().unwrap_or(0) + 1);
            if kind == "fail" {
                counts["fail_statuses"] = json!(counts["fail_statuses"].as_i64().unwrap_or(0) + 1);
            }
        }
    }
    if deduped.is_empty() {
        return ("unknown".into(), 3, counts);
    }
    if counts["fail"].as_i64().unwrap_or(0) > 0 {
        return ("red".into(), 1, counts);
    }
    if counts["pending"].as_i64().unwrap_or(0) > 0 {
        return ("pending".into(), 2, counts);
    }
    if counts["check_runs"].as_i64().unwrap_or(0) == 0 {
        return ("unknown".into(), 3, counts);
    }
    ("green".into(), 0, counts)
}

// ---------------------------------------------------------------------------
// The REST reader (fetch_pr_rest)
// ---------------------------------------------------------------------------

fn map_pr_state(pulls: &Value) -> String {
    if !pulls.get("merged").unwrap_or(&Value::Null).is_null()
        && pulls.get("merged").and_then(Value::as_bool) != Some(false)
        || pulls.get("merged_at").and_then(Value::as_str).is_some()
    {
        return "MERGED".into();
    }
    match pulls.get("state").and_then(Value::as_str).unwrap_or("") {
        "open" => "OPEN".into(),
        "closed" => "CLOSED".into(),
        _ => "UNKNOWN".into(),
    }
}

fn map_mergeable(value: &Value) -> String {
    match value {
        Value::Bool(true) => "MERGEABLE".into(),
        Value::Bool(false) => "CONFLICTING".into(),
        _ => "UNKNOWN".into(),
    }
}

fn probe_json<P: GhProbe>(probe: &P, cwd: &Path, path: &str) -> Result<Value, RestReason> {
    let args = vec!["api".to_string(), path.to_string()];
    let (ok, stdout, stderr) = match probe.run_gh(cwd, &args) {
        Ok(triple) => triple,
        Err(why) => return Err(RestReason::new(why, "")),
    };
    if !ok {
        return Err(rest_reason(&stderr));
    }
    serde_json::from_str::<Value>(&stdout).map_err(|e| {
        RestReason::new(
            format!("gh api {path} returned unparseable output: {e}"),
            "",
        )
    })
}

/// Explain a failed REST read, naming the class that changes what the caller
/// does next: back off (secondary), wait for reset (core), log in (auth),
/// pause (transport), or check the PR number (not found).
///
/// The fleet-backoff record arm Python's classifier carried (`_quota.
/// record_refusal`) lands with the cache port, where the gh_budget door is
/// already in scope.
pub(crate) fn rest_reason(stderr: &str) -> RestReason {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let fallback = lines
        .first()
        .copied()
        .unwrap_or("gh api failed with no message");
    let text = lines.join(" ");
    let matched = |pred: &dyn Fn(&str) -> bool| -> String {
        lines
            .iter()
            .copied()
            .find(|l| pred(l))
            .unwrap_or(fallback)
            .to_string()
    };
    if text.to_lowercase().contains("rate limit") {
        let base = matched(&|l| l.to_lowercase().contains("rate limit"));
        if stderr.contains("HTTP 403") || stderr.contains("HTTP 429") {
            // The one arm that records the fleet backoff; the cache port
            // adds the gh_budget call (see the doc comment above).
            return RestReason::new(
                format!(
                    "{base} | this is the SECONDARY rate limit (request rate, not budget). \
Back off - retrying on a fixed interval sustains the refusal."
                ),
                "secondary",
            );
        }
        return RestReason::new(
            format!(
                "{base} | this is the CORE REST quota (the live exempt bucket). \
Check `gh api rate_limit --jq .resources.core` and wait for its reset."
            ),
            "core",
        );
    }
    if text.contains("gh auth login")
        || text.contains("401")
        || text.to_lowercase().contains("authentication")
    {
        return RestReason::new(
            format!(
                "{base} | this is an AUTHENTICATION failure (gh is not logged in). \
Run `gh auth login`. It is not a verdict about this PR, and any blocker derived \
from this read is not content.",
                base = matched(&|l| l.contains("gh auth login") || l.contains("401"))
            ),
            "",
        );
    }
    if text.to_lowercase().contains("not found") || text.split_whitespace().any(|w| w == "404") {
        return RestReason::new(
            format!(
                "{base} | not found. Check the PR number (and that this branch still has an \
open PR); this is not a verdict about any PR.",
                base = matched(&|l| {
                    l.to_lowercase().contains("not found")
                        || l.split_whitespace().any(|w| w == "404")
                })
            ),
            "",
        );
    }
    RestReason::new(fallback, "")
}

/// The assembled pr_json every caller reads: the keys `run_status` read on
/// the Python side, byte-shape-identical (docs/architecture/
/// pr-status-verdict.md). `slug` is resolved once by the cache layer; the
/// reader never runs git.
pub(crate) fn read_pr<P: GhProbe>(
    probe: &P,
    cwd: &Path,
    slug: &str,
    pr: u64,
) -> Result<Value, RestReason> {
    let pulls = probe_json(probe, cwd, &format!("repos/{slug}/pulls/{pr}"))?;
    let Some(sha) = pulls
        .pointer("/head/sha")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
    else {
        return Err(RestReason::new("the PR read carried no head sha", ""));
    };
    let head_ref = pulls
        .pointer("/head/ref")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    // Check-run pages: total_count bounds the loop; a runner that omits it
    // stops after page 1.
    let mut check_runs: Vec<Value> = Vec::new();
    let mut page = 1;
    loop {
        let payload = probe_json(
            probe,
            cwd,
            &format!("repos/{slug}/commits/{sha}/check-runs?per_page=100&page={page}"),
        )?;
        let Some(rows) = payload.get("check_runs").and_then(Value::as_array) else {
            return Err(RestReason::new(
                "gh api check-runs carried malformed check_runs",
                "",
            ));
        };
        check_runs.extend(rows.iter().cloned());
        let total = payload.get("total_count").and_then(Value::as_i64);
        match total {
            Some(total) if check_runs.len() < total as usize && page < 10 => page += 1,
            _ => break,
        }
    }

    // The zero-job scan: the runs listing, the timed-out relabel, and the
    // shared rule, in process now instead of through the op door.
    let scan =
        zero_job_scan(probe, cwd, slug, &sha, &check_runs).map_err(|e| RestReason::new(e, ""))?;
    let mut run_names: BTreeMap<String, String> = BTreeMap::new();
    for run in &scan.listing {
        if let (Some(id), Some(name)) = (
            run.get("id").and_then(Value::as_i64),
            run.get("name").and_then(Value::as_str),
        ) {
            run_names.insert(id.to_string(), name.to_string());
        }
    }
    let actions_run_re = actions_run_re();
    let mut rollup: Vec<Value> = scan
        .check_runs
        .iter()
        .map(|cr| {
            let details_url = s_str(cr, "details_url");
            let workflow = actions_run_re
                .captures(details_url)
                .and_then(|c| c.get(1))
                .and_then(|m| run_names.get(m.as_str()))
                .cloned()
                .unwrap_or_default();
            let mut row = json!({
                "name": cr.get("name").cloned().unwrap_or(Value::Null),
                "status": cr.get("status").cloned().unwrap_or(json!("")),
                "conclusion": cr.get("conclusion").cloned().unwrap_or(json!("")),
                "startedAt": cr.get("started_at").cloned().unwrap_or(json!("")),
                "detailsUrl": details_url,
                "workflow": workflow,
            });
            if let Some(timeout) = cr.get("timeout") {
                row["timeout"] = timeout.clone();
            }
            row
        })
        .collect();
    rollup.extend(scan.rows);

    // Legacy StatusContexts: a separate check class, so a failed read is
    // always loud - green CheckRuns do not prove an unread context is green.
    let status_payload = probe_json(probe, cwd, &format!("repos/{slug}/commits/{sha}/status"))?;
    let Some(status_rows) = status_payload.get("statuses").and_then(Value::as_array) else {
        return Err(RestReason::new(
            "gh api status carried malformed statuses",
            "",
        ));
    };
    for sc in status_rows {
        rollup.push(json!({
            "context": sc.get("context").cloned().unwrap_or(Value::Null),
            "state": s_str(sc, "state").to_uppercase(),
            "createdAt": sc.get("created_at").cloned().unwrap_or(json!("")),
            "targetUrl": sc.get("target_url").cloned().unwrap_or(json!("")),
        }));
    }

    Ok(json!({
        "state": map_pr_state(&pulls),
        "statusCheckRollup": rollup,
        "headRefOid": sha,
        "headRefName": head_ref,
        "mergeable": map_mergeable(pulls.get("mergeable").unwrap_or(&Value::Null)),
        "mergeStateStatus": pulls.get("mergeable_state").cloned().unwrap_or(Value::Null),
        "baseRefName": pulls.pointer("/base/ref").cloned().unwrap_or(Value::Null),
        // The raw listing rerun recovery and the workflow mapping share.
        "workflowRuns": scan.listing,
    }))
}

fn actions_run_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"/actions/runs/(\d+)").unwrap())
}

// ---------------------------------------------------------------------------
// Failure detail (collect_failures) and the shared job-log cache
// ---------------------------------------------------------------------------

fn ts_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\d{4}-\d{2}-\d{2}T[\d:.]+Z?\s*").unwrap())
}

fn content(line: &str) -> String {
    let no_ts = ts_re().replace(line.trim_end(), "");
    no_ts.trim().to_string()
}

/// The step name on the first `step failed, stopping (fail-fast)` line;
/// fail-fast breaks at the first failure, so the first match is THE failure.
pub(crate) fn failing_step(log: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE
        .get_or_init(|| Regex::new(r"step failed, stopping \(?fail-fast\)?:\s*(.+?)\s*$").unwrap());
    log.lines()
        .map(content)
        .find_map(|line| re.captures(&line).map(|c| c[1].to_string()))
}

/// The failing step's cause, from the one owner: the failure-cause scan that
/// Python reached through the op door, called in process now.
pub(crate) fn first_error(
    log: &str,
    step: Option<&str>,
    window: Option<&Vec<Value>>,
) -> Option<String> {
    let payload = json!({
        "log": log,
        "step": step,
        "window": window,
    });
    let out = failure_cause(&payload);
    out.get("cause").and_then(Value::as_str).map(str::to_string)
}

/// Planned smoke steps with no completion line; None on a log with no
/// `smoke: planned:` prologue (an honest unknown beats a fabricated empty).
pub(crate) fn unreached_runner_steps(log: &str) -> Option<Vec<String>> {
    static PLANNED: OnceLock<Regex> = OnceLock::new();
    static DONE: OnceLock<Regex> = OnceLock::new();
    let planned_re = PLANNED.get_or_init(|| Regex::new(r"^smoke: planned:\s*(.+?)\s*$").unwrap());
    let done_re = DONE
        .get_or_init(|| Regex::new(r"^smoke: (?:pass|fail)\s+\d+(?:\.\d+)?s\s+(.+?)\s*$").unwrap());
    let mut planned: Vec<String> = Vec::new();
    let mut done: Vec<String> = Vec::new();
    for raw in log.lines() {
        let line = content(raw);
        if let Some(c) = planned_re.captures(&line) {
            planned.push(c[1].to_string());
        }
        if let Some(c) = done_re.captures(&line) {
            done.push(c[1].to_string());
        }
    }
    if planned.is_empty() {
        return None;
    }
    Some(
        planned
            .into_iter()
            .filter(|name| !done.contains(name))
            .collect(),
    )
}

/// The step whose conclusion is `failure`, or None.
fn failed_job_step(steps: &[Value]) -> Option<String> {
    steps
        .iter()
        .find(|s| s_str(s, "conclusion").to_lowercase() == "failure")
        .and_then(|s| s.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Steps after the failed one whose OWN conclusion is `skipped`: the work
/// fail-fast never reached. `Post *` and `Complete job` are GitHub cleanup,
/// never counted; no failed step yields [] (position says nothing).
pub(crate) fn unreached_job_steps(steps: &[Value]) -> Vec<String> {
    let failed_at = steps
        .iter()
        .position(|s| s_str(s, "conclusion").to_lowercase() == "failure");
    let Some(failed_at) = failed_at else {
        return Vec::new();
    };
    steps[failed_at + 1..]
        .iter()
        .filter_map(|s| {
            let name = s_str(s, "name");
            if name.is_empty() || name.starts_with("Post ") || name == "Complete job" {
                return None;
            }
            if s_str(s, "conclusion").to_lowercase() != "skipped" {
                return None;
            }
            Some(name.to_string())
        })
        .collect()
}

fn check_name(check: &Value) -> String {
    let name = s_str(check, "name");
    if !name.is_empty() {
        return name.to_string();
    }
    let context = s_str(check, "context");
    if !context.is_empty() {
        return context.to_string();
    }
    "(unnamed check)".to_string()
}

/// (owner, repo, job_id) from a CheckRun's detailsUrl, else None: a
/// StatusContext carries a targetUrl no jobs API can serve.
fn job_ref(check: &Value) -> Option<(String, String, String)> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"^https?://[^/]+/([^/]+)/([^/]+)/actions/runs/\d+/job/(\d+)").unwrap()
    });
    let url = {
        let details = s_str(check, "detailsUrl");
        if !details.is_empty() {
            details
        } else {
            s_str(check, "targetUrl")
        }
    };
    re.captures(url)
        .map(|c| (c[1].to_string(), c[2].to_string(), c[3].to_string()))
}

/// One completed attempt's log, cached by job id: `<cache>/job-<slug-key>-<job_id>.json`
/// holds `{ts, log}`. A job id names one attempt, so the file never needs a
/// TTL; the gc reaper's window bounds it. A failed read is never cached.
pub(crate) fn job_log<P: GhProbe>(
    probe: &P,
    cwd: &Path,
    slug_key: &str,
    owner: &str,
    repo: &str,
    job_id: &str,
) -> Result<String, String> {
    let dir = crate::agents_config::pr_status_cache_dir(cwd)
        .ok_or_else(|| "no pr-status cache dir".to_string())?;
    let path: PathBuf = dir.join(format!("job-{slug_key}-{job_id}.json"));
    if let Ok(row) = std::fs::read_to_string(&path) {
        if let Ok(parsed) = serde_json::from_str::<Value>(&row) {
            if let Some(log) = parsed.get("log").and_then(Value::as_str) {
                return Ok(log.to_string());
            }
        }
    }
    let args = vec![
        "api".to_string(),
        format!("repos/{owner}/{repo}/actions/jobs/{job_id}/logs"),
    ];
    let (ok, stdout, stderr) = probe.run_gh(cwd, &args)?;
    if !ok {
        return Err(if stderr.trim().is_empty() {
            "gh error".to_string()
        } else {
            stderr.trim().to_string()
        });
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut f) = std::fs::File::create(&path) {
        let _ = f.write_all(json!({"ts": ts, "log": stdout}).to_string().as_bytes());
    }
    Ok(stdout)
}

/// Detail entries for the failing rollup rows, loudest facts first. The log
/// answers first; the job object's steps[] only when the log cannot. A row
/// that is not an Actions job is named with no log claim; a fetch failure
/// names its class rather than vanishing. Capped at MAX_DETAILED_FAILURES
/// with an explicit truncation entry. `known` maps job id -> a prior entry
/// for the SAME job, replayed with no read (a job id is minted per attempt).
pub(crate) fn collect_failures<P: GhProbe>(
    probe: &P,
    cwd: &Path,
    slug_key: &str,
    failing: &[Value],
    known: &BTreeMap<String, Value>,
) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for check in failing.iter().take(MAX_DETAILED_FAILURES) {
        let mut entry = json!({"check": check_name(check)});
        if let Some(timeout) = check.get("timeout").and_then(Value::as_str) {
            entry["first_error"] = json!(timeout);
        }
        let Some((owner, repo, job_id)) = job_ref(check) else {
            entry["detail"] = json!("not an Actions job (commit status); no job log to read");
            out.push(entry);
            continue;
        };
        entry["job_id"] = json!(job_id);
        if let Some(prior) = known.get(&job_id) {
            out.push(prior.clone());
            continue;
        }
        let log_text = job_log(probe, cwd, slug_key, &owner, &repo, &job_id);
        match log_text {
            Err(why) => {
                entry["detail"] = json!(format!("log unavailable: {}", truncate(&why, 160)));
            }
            Ok(log) if log.is_empty() => {
                // No log text (an empty log): the job object still names WHICH
                // step failed beside the consequence the unreached steps spell out.
                let steps = fetch_job_steps(probe, cwd, &owner, &repo, &job_id);
                let unreached = unreached_job_steps(&steps);
                if !unreached.is_empty() {
                    entry["unreached_steps"] = json!(unreached);
                }
                if let Some(step) = failed_job_step(&steps) {
                    if entry.get("step").is_none() {
                        entry["step"] = json!(step);
                    }
                }
            }
            Ok(log) => {
                if let Some(step) = failing_step(&log) {
                    entry["step"] = json!(step);
                    if let Some(err) = first_error(&log, Some(&step), None) {
                        if entry.get("first_error").is_none() {
                            entry["first_error"] = json!(err);
                        }
                    }
                }
                match unreached_runner_steps(&log) {
                    Some(runner_unreached) if !runner_unreached.is_empty() => {
                        entry["unreached_steps"] = json!(runner_unreached);
                    }
                    Some(_) => {}
                    None => {
                        // No runner lines: a plain multi-step job, so the
                        // GitHub steps after the failed one are the unreached work.
                        let steps = fetch_job_steps(probe, cwd, &owner, &repo, &job_id);
                        let unreached = unreached_job_steps(&steps);
                        if !unreached.is_empty() {
                            entry["unreached_steps"] = json!(unreached);
                        }
                        if let Some(step) = failed_job_step(&steps) {
                            if entry.get("step").is_none() {
                                entry["step"] = json!(step);
                            }
                            if entry.get("first_error").is_none() {
                                if let Some(err) = first_error(&log, Some(&step), Some(&steps)) {
                                    entry["first_error"] = json!(err);
                                }
                            }
                        }
                    }
                }
            }
        }
        out.push(entry);
    }
    if failing.len() > MAX_DETAILED_FAILURES {
        out.push(json!({"check": format!(
            "({} more failing check(s) not detailed)",
            failing.len() - MAX_DETAILED_FAILURES
        )}));
    }
    out
}

fn fetch_job_steps<P: GhProbe>(
    probe: &P,
    cwd: &Path,
    owner: &str,
    repo: &str,
    job_id: &str,
) -> Vec<Value> {
    let args = vec![
        "api".to_string(),
        format!("repos/{owner}/{repo}/actions/jobs/{job_id}"),
    ];
    let Ok((ok, stdout, _)) = probe.run_gh(cwd, &args) else {
        return Vec::new();
    };
    if !ok {
        return Vec::new();
    }
    serde_json::from_str::<Value>(&stdout)
        .ok()
        .and_then(|job| job.get("steps").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

fn truncate(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_string();
    }
    let mut end = cap;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

pub(crate) mod compose;
pub(crate) mod seams;
#[cfg(test)]
mod tests;
