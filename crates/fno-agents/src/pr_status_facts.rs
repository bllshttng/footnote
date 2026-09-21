//! Read-only `fno do pr status` facts riding the `authorized-merge` op door.
//!
//! Two false readings cost workers and the crown on 2026-09-18:
//! `ready` answered "does it conflict" while GitHub's
//! `mergeable_state` answers "will GitHub merge it", and the failure line
//! named stderr from a PASSING test because the block scanner only matched
//! `::group::` while downloaded job logs spell it `##[group]`, so the scan
//! ran from line 1 of the whole job log. The logic lives here (law
//! d-a9cddc93: a `cli/src/fno` repair is transport only, at most 30 added
//! Python lines); Python reaches it as
//! `{"op": "status-merge-blocker"|"status-failure-cause", ...}` on the same
//! door the hold and grant ops ride.

use crate::heal::{cargo_test_names, pytest_nodeids};
use crate::tick_ledger::parse_rfc3339_unix;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The gh probe seam, shaped like `authorized_merge::Probes::run_gh` but
/// returning the streams SEPARATE: this module parses stdout as JSON, and a
/// wrapped gh whose stderr carries machine-local noise (config warnings)
/// would corrupt a combined string.
pub(crate) trait GhProbe {
    fn run_gh(&self, cwd: &Path, args: &[String]) -> Result<(bool, String, String), String>;
}

pub(crate) struct RealGhProbe;

impl GhProbe for RealGhProbe {
    fn run_gh(&self, cwd: &Path, args: &[String]) -> Result<(bool, String, String), String> {
        let out = Command::new("gh")
            .args(args)
            .current_dir(cwd)
            .output()
            .map_err(|error| error.to_string())?;
        Ok((
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ))
    }
}

/// Dispatch one `status-` op from an `authorized-merge` payload. Always
/// answers with a JSON receipt; the verb's exit status answers only whether
/// the op RAN.
pub fn run_op(op: &str, payload: &Value) -> String {
    match op {
        "status-merge-blocker" => merge_blocker(&RealGhProbe, payload).to_string(),
        "status-failure-cause" => failure_cause(payload).to_string(),
        "status-zero-job-runs" => zero_job_runs_op(&RealGhProbe, payload).to_string(),
        "status-cache-key" => status_cache_key(payload).to_string(),
        other => json!({"error": format!("unknown op {other}")}).to_string(),
    }
}


// ---------------------------------------------------------------------------
// status-cache-key

/// The status row's cache key, minted from every fact the merge decision
/// reads: head sha, PR state, the PR's dispatch-hold word, every
/// live merge-slot row in the repo's space, and the review-evidence lines
/// naming this head. A hold release, a slot move, a merge, or a fresh
/// attestation changes the key, so a row written before the change can
/// never serve inside the TTL. Falls back to the head-only word on any
/// ingredient fault: an unreadable ingredient degrades to today's key
/// shape, never to a wrong one.
pub(crate) fn status_cache_key(payload: &Value) -> Value {
    use sha2::{Digest, Sha256};
    use std::process::Command;

    let cwd = PathBuf::from(payload.get("cwd").and_then(Value::as_str).unwrap_or("."));
    let pr = payload.get("pr").and_then(Value::as_u64).unwrap_or(0);
    let head = payload.get("head_sha").and_then(Value::as_str).unwrap_or("");
    let state = payload.get("pr_state").and_then(Value::as_str).unwrap_or("");
    let slug = payload.get("slug").and_then(Value::as_str).unwrap_or("");

    // The repo's live merge authority: a config flip must rekey every row
    // of the repo, or a cached `merge_authority: false` outlives the flip
    // inside the TTL (the PR 2182 specimen).
    let enabled = crate::agents_config::auto_merge_enabled(&cwd);
    let dispatch = crate::agents_config::auto_merge_grant_dispatches(&cwd);
    let mut material = format!("{head}|{state}|{enabled}|{dispatch}|");

    // The PR's dispatch-hold word, through the same probe the merge path
    // reads: exit 0 clear, 3 held, anything else unreadable.
    let hold_word = Command::new("fno")
        .args(["do", "pr", "hold-check", &pr.to_string()])
        .current_dir(&cwd)
        .output()
        .ok()
        .map(|out| {
            format!(
                "{:?}:{}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            )
        })
        .unwrap_or_else(|| "spawn-failed".to_string());
    material.push_str(&hold_word);
    material.push('|');

    // Every live merge-slot row in this repo's space: one store, the db the
    // claim verb reads, so a take or a move rekeys every queued PR.
    if let Ok(rows) = crate::claim_store::list_db(Some("merge-slot:"), false, None) {
        material.push_str(rows.to_string().as_str());
    }
    material.push('|');

    // Review-evidence lines at this head (attestations, coverage rows,
    // findings): a fresh verdict on an unchanged head must rekey.
    let journal = crate::paths::events_path(&cwd);
    let text = crate::events_store::review_text(&journal);
    if !head.is_empty() {
        let mut count = 0usize;
        let mut last = String::new();
        for line in text.lines().filter(|l| l.contains(head)) {
            count += 1;
            last = line.to_string();
        }
        let mut hasher = Sha256::new();
        hasher.update(last.as_bytes());
        let digest = format!("{:x}", hasher.finalize());
        material.push_str(&format!("{count}|{}", &digest[..12.min(digest.len())]));
    }

    let mut hasher = Sha256::new();
    hasher.update(material.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    json!({"key": format!("{slug}-{pr}-{}", &digest[..12])})
}

// ---------------------------------------------------------------------------
// status-merge-blocker
// ---------------------------------------------------------------------------

/// Map GitHub's REST `mergeable_state` onto the ready-blocker vocabulary.
///
/// `mergeable` answers "does it conflict"; `mergeable_state` answers "will
/// GitHub merge it". The states an existing conjunct already names are
/// silent here (`dirty` -> `not_mergeable_conflicting`, a null `mergeable`
/// -> `not_mergeable_unknown`, `unstable` -> the CI conjunct, `has_hooks`
/// and `clean` -> GitHub will merge); the states only this read sees do
/// block. `blocked` is the ruleset / protection / required-review hold, and
/// on it - and only it - the live rules read names what is missing. A rules
/// read that fails or explains nothing answers `missing_required_checks:
/// null` and says so in `source`; the blocker is never dropped.
pub(crate) fn merge_blocker<P: GhProbe>(probes: &P, payload: &Value) -> Value {
    // The Python transport spreads the whole pr_json into the payload, so
    // keys arrive under their REST names; the flat spellings are accepted
    // too (the tests use them).
    let state = payload
        .get("merge_state")
        .or_else(|| payload.get("mergeStateStatus"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let cwd = PathBuf::from(payload.get("cwd").and_then(Value::as_str).unwrap_or("."));
    let base = payload
        .get("base_ref")
        .or_else(|| payload.get("baseRefName"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let (mut blockers, missing, source) = match state {
        "clean" | "has_hooks" | "unstable" | "dirty" | "unknown" | "" => {
            (Vec::new(), Value::Null, format!("mergeable_state {state}"))
        }
        "behind" => (
            vec!["github_behind".to_string()],
            Value::Null,
            "mergeable_state behind".to_string(),
        ),
        "draft" => (
            vec!["github_draft".to_string()],
            Value::Null,
            "mergeable_state draft".to_string(),
        ),
        "blocked" => {
            let pr = payload.get("pr").and_then(Value::as_u64).unwrap_or(0);
            let missing = blocked_missing(probes, &cwd, base, payload.get("rollup"), pr);
            match missing {
                Some(missing) => (
                    vec!["github_blocked".to_string()],
                    json!(missing),
                    format!("rules/branches/{base}"),
                ),
                None => (
                    vec!["github_blocked".to_string()],
                    Value::Null,
                    "the rules read failed or names no unsatisfied rule; the block stands"
                        .to_string(),
                ),
            }
        }
        other => (
            vec![format!("github_merge_state_{other}")],
            Value::Null,
            format!("mergeable_state {other}"),
        ),
    };
    blockers.sort();
    blockers.dedup();
    json!({
        "state": payload
            .get("merge_state")
            .or_else(|| payload.get("mergeStateStatus"))
            .cloned()
            .unwrap_or(Value::Null),
        "blockers": blockers,
        "missing_required_checks": missing,
        "source": source,
    })
}

/// Which required checks or reviews the live rules read holds unmet, for a
/// PR whose `mergeable_state` reads `blocked`. `None` when the read failed
/// or no rule in it explains the hold.
fn blocked_missing<P: GhProbe>(
    probes: &P,
    cwd: &Path,
    base: &str,
    rollup: Option<&Value>,
    pr: u64,
) -> Option<Vec<String>> {
    if base.is_empty() {
        return None;
    }
    let args = vec![
        "api".to_string(),
        format!("repos/{{owner}}/{{repo}}/rules/branches/{base}"),
    ];
    let (ok, stdout, _stderr) = probes.run_gh(cwd, &args).ok()?;
    if !ok {
        return None;
    }
    let rules: Value = serde_json::from_str(&stdout).ok()?;
    let rules = rules.as_array()?;
    let rows = rollup.and_then(Value::as_array);
    let mut missing: Vec<String> = Vec::new();
    let mut explained = false;
    for rule in rules {
        let params = rule.get("parameters");
        if rule.get("type").and_then(Value::as_str) == Some("required_status_checks") {
            explained = true;
            let required = params
                .and_then(|p| p.get("required_status_checks"))
                .and_then(Value::as_array);
            for check in required.into_iter().flatten() {
                let context = check.get("context").and_then(Value::as_str).unwrap_or("");
                if context.is_empty() {
                    continue;
                }
                if !context_concluded(rows, context) {
                    missing.push(context.to_string());
                }
            }
        }
        if rule.get("type").and_then(Value::as_str) == Some("pull_request") {
            let required_reviews = params
                .and_then(|p| p.get("required_approving_review_count"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if required_reviews > 0 {
                explained = true;
                // The rule's existence is not the hold; GitHub's own
                // reviewDecision answers whether the requirement is met.
                // An unreadable probe names the requirement - the safe
                // direction is naming a review that exists, never staying
                // quiet about one that is missing.
                if !reviews_satisfied(probes, cwd, pr) {
                    missing.push("required_review".to_string());
                }
            }
        }
    }
    if !explained || missing.is_empty() {
        // No rule, or every named requirement already passes: the read
        // explains nothing about the hold, and null says so honestly.
        return None;
    }
    missing.sort();
    missing.dedup();
    Some(missing)
}

/// GitHub's own review decision for the PR: `APPROVED` only when the
/// requirement is met.
fn reviews_satisfied<P: GhProbe>(probes: &P, cwd: &Path, pr: u64) -> bool {
    if pr == 0 {
        return false;
    }
    let args = vec![
        "pr".to_string(),
        "view".to_string(),
        pr.to_string(),
        "--json".to_string(),
        "reviewDecision".to_string(),
        "--jq".to_string(),
        ".reviewDecision".to_string(),
    ];
    matches!(probes.run_gh(cwd, &args), Ok((true, stdout, _)) if stdout.trim() == "APPROVED")
}

/// Whether `context` has a rollup row whose conclusion is a passing one.
/// Rows arrive in both REST shapes: check-runs carry `name`/`conclusion`
/// (lowercase), commit statuses carry `context`/`state` (uppercase).
fn context_concluded(rows: Option<&Vec<Value>>, context: &str) -> bool {
    rows.unwrap_or(&Vec::new()).iter().any(|row| {
        let name = row
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| row.get("context").and_then(Value::as_str))
            .unwrap_or("");
        if name != context {
            return false;
        }
        let conclusion = row
            .get("conclusion")
            .and_then(Value::as_str)
            .or_else(|| row.get("state").and_then(Value::as_str))
            .unwrap_or("");
        matches!(
            conclusion.to_uppercase().as_str(),
            "SUCCESS" | "NEUTRAL" | "SKIPPED"
        )
    })
}

// ---------------------------------------------------------------------------
// status-zero-job-runs
// ---------------------------------------------------------------------------

/// One Actions run that completed as `failure` with zero jobs: GitHub failed
/// to parse the workflow file, the run minted no check run, and every
/// check-run-shaped reader would otherwise skip it.
pub(crate) struct ZeroJobRun {
    pub(crate) path: String,
    pub(crate) url: String,
    pub(crate) conclusion: String,
    pub(crate) created_at: String,
}

/// The runs that failed before minting a job. A run a check run links to
/// already owns its row; a run whose newest verdict at its path is not a
/// completed failure is superseded or green. `jobs_total` is the read that
/// proves the zero, kept a seam so the rule stays pure.
pub(crate) fn zero_job_failures(
    runs: &[Value],
    check_runs: &[Value],
    jobs_total: &dyn Fn(u64) -> Result<u64, String>,
) -> Result<Vec<ZeroJobRun>, String> {
    let linked: HashSet<String> = check_runs
        .iter()
        .filter_map(|cr| {
            let url = cr
                .get("details_url")
                .and_then(Value::as_str)
                .or_else(|| cr.get("html_url").and_then(Value::as_str))
                .unwrap_or("");
            crate::heal::run_id(url)
        })
        .collect();
    // Newest run per workflow path wins, as parse_failing_run_ids does: a
    // newer run of the same workflow supersedes an older failure.
    let mut newest: BTreeMap<&str, &Value> = BTreeMap::new();
    for run in runs {
        let Some(path) = run.get("path").and_then(Value::as_str) else {
            continue;
        };
        let Some(id) = run.get("id").and_then(Value::as_u64) else {
            continue;
        };
        let newer = newest
            .get(path)
            .is_none_or(|prev| prev.get("id").and_then(Value::as_u64).unwrap_or(0) < id);
        if newer {
            newest.insert(path, run);
        }
    }
    let mut out = Vec::new();
    for (path, run) in newest {
        let status = run.get("status").and_then(Value::as_str).unwrap_or("");
        let conclusion = run.get("conclusion").and_then(Value::as_str).unwrap_or("");
        if status != "completed" || !matches!(conclusion, "failure" | "startup_failure") {
            continue;
        }
        let Some(id) = run.get("id").and_then(Value::as_u64) else {
            continue;
        };
        if linked.contains(&id.to_string()) {
            continue;
        }
        if jobs_total(id)? != 0 {
            continue;
        }
        out.push(ZeroJobRun {
            path: path.to_string(),
            url: run
                .get("html_url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            conclusion: conclusion.to_string(),
            created_at: run
                .get("created_at")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        });
    }
    Ok(out)
}

/// The `status-zero-job-runs` op: `slug` + the raw runs/check-runs arrays
/// in, the Python rollup rows out. `jobs_total` rides the gh probe seam.
fn zero_job_runs_op<P: GhProbe>(probes: &P, payload: &Value) -> Value {
    let Some(slug) = payload
        .get("slug")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return json!({"error": "status-zero-job-runs needs a non-empty slug"});
    };
    let cwd = PathBuf::from(payload.get("cwd").and_then(Value::as_str).unwrap_or("."));
    let Some(check_runs) = payload.get("check_runs").and_then(Value::as_array) else {
        return json!({"error": "status-zero-job-runs needs a check_runs array"});
    };
    // The op owns the runs listing: paginated, because a busy head carries
    // more runs than one page and a zero-job failure past page 1 must still
    // read red. No caller passes a pre-read page - one listing, one reader.
    let Some(sha) = payload
        .get("sha")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return json!({"error": "status-zero-job-runs needs a non-empty sha"});
    };
    let path = format!("repos/{slug}/actions/runs?head_sha={sha}&per_page=100");
    let runs: Vec<Value> = match probes
        .run_gh(
            &cwd,
            &[
                "api".to_string(),
                path,
                "--paginate".to_string(),
                "--slurp".to_string(),
            ],
        )
        .and_then(|(ok, stdout, _stderr)| {
            if ok {
                Ok(stdout)
            } else {
                Err("the runs listing read failed".to_string())
            }
        })
        .and_then(|stdout| {
            serde_json::from_str::<Value>(&stdout)
                .map_err(|e| format!("the runs listing was unparseable: {e}"))
        })
        .map(|parsed| match parsed {
            Value::Array(pages) => pages,
            other => vec![other],
        })
        .map(|pages| {
            let mut runs: Vec<Value> = Vec::new();
            for page in pages {
                if let Some(list) = page.get("workflow_runs").and_then(|r| r.as_array()) {
                    runs.extend(list.iter().cloned());
                }
            }
            runs
        }) {
        Ok(runs) => runs,
        Err(err) => return json!({ "error": err }),
    };
    let jobs_total = |id: u64| -> Result<u64, String> {
        let args = vec![
            "api".to_string(),
            format!("repos/{slug}/actions/runs/{id}/jobs?per_page=1"),
        ];
        let (ok, stdout, _stderr) = probes.run_gh(&cwd, &args)?;
        if !ok {
            return Err(format!("the jobs read for run {id} failed"));
        }
        serde_json::from_str::<Value>(&stdout)
            .map_err(|e| format!("the jobs read for run {id} was unparseable: {e}"))?
            .get("total_count")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("the jobs read for run {id} carried no total_count"))
    };
    match zero_job_failures(&runs, check_runs, &jobs_total) {
        Err(err) => json!({"error": err}),
        Ok(rows) => json!({
            "rows": rows
                .iter()
                .map(|r| {
                    json!({
                        "name": r.path,
                        "status": "completed",
                        "conclusion": r.conclusion,
                        "startedAt": r.created_at,
                        "detailsUrl": r.url,
                        "workflow": r.path,
                    })
                })
                .collect::<Vec<_>>(),
            // The same listing, for the caller's workflow-name mapping.
            "listing": runs,
        }),
    }
}

// ---------------------------------------------------------------------------
// status-failure-cause
// ---------------------------------------------------------------------------

/// The window argument arrives two ways: a `[started_at, completed_at]`
/// pair, or the job's `steps[]` (whose failed step carries the pair). The
/// Python transport passes the steps list it already holds, so the failed
/// step's derivation lives here and not in counted Python lines. One second
/// of slack rides the end (the steps API has second precision).
fn window_pair(window: Option<&Vec<Value>>) -> Option<(u64, u64)> {
    let window = window?;
    let first = window.first()?;
    if first.is_object() {
        let failed = window
            .iter()
            .find(|step| step.get("conclusion").and_then(Value::as_str) == Some("failure"))?;
        let start = parse_rfc3339_unix(failed.get("started_at").and_then(Value::as_str)?)?;
        let end = parse_rfc3339_unix(failed.get("completed_at").and_then(Value::as_str)?)?;
        return Some((start, end));
    }
    let start = parse_rfc3339_unix(first.as_str()?)?;
    let end = parse_rfc3339_unix(window.get(1).and_then(Value::as_str)?)?;
    Some((start, end))
}

/// The cause of a failed job, from its own log: the harness verdict names
/// it (cargo test panic, pytest assertion), never an `error:`-shaped stderr
/// line from an earlier, PASSING step (the 2026-09-18 misread).
///
/// The block is scoped, in order of strength: the failed step's
/// `[started_at, completed_at]` window when the caller carries it (the one
/// scope that survives a plain Actions job, whose group line spells
/// `Run <command>`, not the step's name); else after the last
/// `##[group]<step>` / `::group::<step>` / `=== <step> ===` marker before
/// the step-failed line; else the whole log up to that line. Cargo and
/// pytest verdict parsing is `heal`'s, raised to `pub(crate)` - no third
/// parser. `error_line` (the ported `_ERROR_PATTERNS` list) and the block
/// tail are the fallbacks.
pub(crate) fn failure_cause(payload: &Value) -> Value {
    let log = payload.get("log").and_then(Value::as_str).unwrap_or("");
    let step = payload.get("step").and_then(Value::as_str);
    let window = window_pair(payload.get("window").and_then(Value::as_array));

    let ts_re = Regex::new(r"^(\d{4}-\d{2}-\d{2}T[\d:.]+Z?)\s").expect("static regex");
    let lines: Vec<(Option<u64>, String)> = log
        .lines()
        .map(|raw| {
            let stripped = ts_re.replace(raw, "").trim().to_string();
            let ts = ts_re.captures(raw).and_then(|c| parse_rfc3339_unix(&c[1]));
            (ts, stripped)
        })
        .collect();

    // The block: (start index, end index) into `lines`, end exclusive.
    let step_failed = Regex::new(r"step failed, stopping \(?fail-fast\)?:").expect("static regex");
    let end = lines
        .iter()
        .position(|(_, content)| step_failed.is_match(content))
        .unwrap_or(lines.len());
    let block_range = if let Some((w_start, w_end)) = window {
        // Timestamped-window scope. The steps API has second precision, so
        // the end gets one second of slack; a line with no parseable
        // timestamp cannot prove it belongs and is dropped.
        let mut block_start = lines.len();
        let mut block_end = 0;
        for (i, (ts, _)) in lines.iter().enumerate() {
            match ts {
                Some(ts) if *ts >= w_start && *ts <= w_end + 1 => {
                    block_start = block_start.min(i);
                    block_end = block_end.max(i + 1);
                }
                _ => {}
            }
        }
        if block_start < block_end {
            block_start..block_end
        } else {
            0..0
        }
    } else if let Some(step) = step {
        let mut block_start = 0;
        let group_re = Regex::new(r"^(?:##\[group\]|::group::)\s*(.+)$").expect("static regex");
        let banner_re = Regex::new(r"^===\s*(.+?)\s*===$").expect("static regex");
        for i in (0..end).rev() {
            let name = group_re
                .captures(&lines[i].1)
                .or_else(|| banner_re.captures(&lines[i].1))
                .and_then(|c| c.get(1))
                .map(|m| m.as_str().trim().to_string());
            if name.as_deref() == Some(step) {
                block_start = i + 1;
                break;
            }
        }
        block_start..end
    } else {
        0..end
    };
    let block: Vec<String> = lines[block_range.start..block_range.end]
        .iter()
        .map(|(_, content)| content.clone())
        .filter(|content| {
            !content.is_empty() && !content.starts_with("::") && !content.starts_with("smoke: ")
        })
        .collect();
    let block_text = block.join("\n");

    // Cargo first: the FAILED test name, then its own panic line.
    if let Some(cause) = cargo_cause(&block_text) {
        return finished(Some(cause), "cargo");
    }
    // Pytest next: the first FAILED node id plus its contiguous E lines,
    // note-prefixed lines sunk last (the 2026-09-18 misread: the assertion's
    // advisory stderr head read as the cause while the real violation sat
    // two E lines down).
    let ids = pytest_nodeids(&block_text);
    if let Some(nodeid) = ids.first() {
        let e_re = Regex::new(r"^E\s{2,}(.*)$").expect("static regex");
        let mut assert_lines: Vec<String> = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        let mut in_e = false;
        for row in &block {
            if let Some(caps) = e_re.captures(row) {
                in_e = true;
                let text = caps[1].trim().to_string();
                if text.starts_with("note:") {
                    notes.push(text);
                } else {
                    assert_lines.push(text);
                }
            } else if in_e {
                // The E block is contiguous; a later failure's E lines must
                // not join this cause.
                break;
            }
        }
        if !assert_lines.is_empty() || !notes.is_empty() {
            assert_lines.extend(notes);
            return finished(
                Some(format!("FAILED {nodeid}: {}", assert_lines.join(" | "))),
                "pytest",
            );
        }
        return finished(Some(format!("FAILED {nodeid}")), "pytest");
    }
    // Fallback: the earliest error-shaped line, then the block tail.
    let patterns = error_patterns();
    for row in &block {
        if patterns.iter().any(|p| p.is_match(row)) {
            return finished(Some(row.to_string()), "error_line");
        }
    }
    finished(block.last().cloned(), "tail")
}

fn cargo_cause(block_text: &str) -> Option<String> {
    let names = cargo_test_names(block_text);
    if let Some(name) = names.first() {
        let panic_re = Regex::new(&format!(
            r"^thread '{}'(?:\s*\(\d+\))?\s*panicked at (.+?):(\d+):\d+:",
            regex::escape(name)
        ))
        .expect("static regex");
        return panic_cause(block_text, &panic_re)
            .map(|(file, line, message)| {
                format!("test {name} panicked at {file}:{line}: {message}")
            })
            .or_else(|| {
                Some(format!(
                    "test {name} FAILED (its panic line was not in the block)"
                ))
            });
    }
    // A panic with no FAILED line (a build script): the panic alone.
    let any_panic = Regex::new(r"^thread '[^']*'(?:\s*\(\d+\))?\s*panicked at (.+?):(\d+):\d+:")
        .expect("static regex");
    panic_cause(block_text, &any_panic)
        .map(|(file, line, message)| format!("panicked at {file}:{line}: {message}"))
}

/// The panic line's location plus its message: the lines after it up to a
/// blank line, a `note:`, or `stack backtrace:`.
fn panic_cause(block_text: &str, panic_re: &Regex) -> Option<(String, String, String)> {
    let lines: Vec<&str> = block_text.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if let Some(caps) = panic_re.captures(line) {
            let mut message: Vec<String> = Vec::new();
            for next in lines.iter().skip(i + 1) {
                let trimmed = next.trim();
                if trimmed.is_empty()
                    || trimmed.starts_with("note:")
                    || trimmed.starts_with("stack backtrace:")
                {
                    break;
                }
                message.push(trimmed.to_string());
            }
            return Some((caps[1].to_string(), caps[2].to_string(), message.join(" ")));
        }
    }
    None
}

/// The ported `_ERROR_PATTERNS` list (most specific first per line; the
/// earliest matching line in the block wins).
fn error_patterns() -> Vec<Regex> {
    [
        r"\bFAILED\b",
        r"^E\s{2,}",
        r"\b[EFNW]\d{3}\b",
        r"\bTraceback \(most recent call last\)",
        r"(?i)\berror:",
        r"\bERROR\b",
        r"\bAssertionError\b",
    ]
    .iter()
    .map(|p| Regex::new(p).expect("static regex"))
    .collect()
}

/// The 480-character cause cap; cut text is announced, never silent.
fn finished(cause: Option<String>, source: &str) -> Value {
    const CAUSE_CAP: usize = 480;
    let mut truncated = false;
    let cause = cause.map(|text| {
        if text.chars().count() <= CAUSE_CAP {
            return text;
        }
        let cut = text.chars().count() - CAUSE_CAP;
        truncated = true;
        format!(
            "{} [+{cut} chars cut]",
            text.chars().take(CAUSE_CAP).collect::<String>()
        )
    });
    json!({"cause": cause, "source": source, "truncated": truncated})
}

#[cfg(test)]
mod tests;
