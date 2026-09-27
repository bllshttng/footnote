//! The DoneAwaitingMerge classifier: main's red per workflow, and a crown's
//! ruling hold, as two ways to prove a worker's merge is blocked by someone
//! else's decision rather than its own breakage. Split out (the file-budget
//! remedy) from the middle of `loopcheck.rs`, which is shrink-only.

use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;

/// How many latest completed main runs to scan, per workflow. Sized to cover
/// cli-ci's lag: this repo's slowest workflow (smoke, smoke-pytest,
/// smoke-rest) takes 16 to 30 minutes against a main that merges every few
/// minutes, so its newest COMPLETED run can sit several commits behind the
/// newest run of a fast workflow. 40 runs covers about six commits at 4 to 5
/// runs per commit, with margin. Bounded so the per-fire gh cost stays
/// constant.
const MAIN_RUN_LOOKBACK: usize = 40;

/// databaseIds of the newest-per-workflow FAILING runs from a `gh run list
/// --json databaseId,conclusion,headSha,workflowName` payload (assumed
/// newest-first, as `gh run list` returns them). For each workflow, the
/// first run with a real verdict (`success` or `failure`) is that workflow's
/// latest verdict; a `cancelled`/`skipped`/`neutral` run produced no verdict
/// and is skipped WITHOUT consuming that workflow's slot, so an older
/// completed run for the same workflow is consulted instead.
fn parse_failing_run_ids(run_list: &Value) -> Vec<i64> {
    let Some(arr) = run_list.as_array() else {
        return Vec::new();
    };
    let mut decided: BTreeSet<&str> = BTreeSet::new();
    let mut ids = Vec::new();
    for run in arr {
        let workflow = run
            .get("workflowName")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if workflow.is_empty() || decided.contains(workflow) {
            continue;
        }
        let conclusion = run.get("conclusion").and_then(|v| v.as_str()).unwrap_or("");
        if conclusion != "success" && conclusion != "failure" {
            continue; // no verdict ran; an older run for this workflow decides
        }
        decided.insert(workflow);
        if conclusion == "failure" {
            if let Some(id) = run.get("databaseId").and_then(|v| v.as_i64()) {
                ids.push(id);
            }
        }
    }
    ids
}

/// Failing job names from a `gh run view <id> --json jobs` payload. The
/// `jobs[].name` field is the same namespace as `gh pr checks .name` (both
/// are the check-run/job name), so a name from here matches a PR
/// failing-check name.
fn parse_failing_job_names(jobs_json: &Value) -> Vec<String> {
    let Some(jobs) = jobs_json.get("jobs").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    jobs.iter()
        .filter(|j| j.get("conclusion").and_then(|v| v.as_str()) == Some("failure"))
        .filter_map(|j| j.get("name").and_then(|v| v.as_str()).map(str::to_string))
        .collect()
}

/// The strict subset rule: main's failing set must COVER every failing PR
/// check. Empty PR-failing is never eligible (that is the DonePRGreen path,
/// not here); any PR-unique failing check blocks the terminal (the
/// session's own breakage).
pub(super) fn is_pre_existing_main_red(pr_failing: &[String], main_failing: &[String]) -> bool {
    if pr_failing.is_empty() {
        return false;
    }
    pr_failing.iter().all(|c| main_failing.contains(c))
}

/// Union of failing job names on main's LATEST VERDICT per workflow. Not
/// necessarily main HEAD: a slow workflow's newest completed run can trail
/// several commits behind a fast workflow's. Fail-CLOSED: any gh error,
/// non-zero exit, malformed JSON, or zero completed runs returns `None`
/// (unknown -> the caller holds as today). A clean read with no failures
/// returns `Some(empty)` -> the subset rule then fails and the caller holds;
/// only positive proof fires the terminal.
pub(crate) fn main_head_failing_checks(gh_bin: &str, cwd: &Path) -> Option<Vec<String>> {
    let limit = MAIN_RUN_LOOKBACK.to_string();
    let list_out = match super::bounded_read(
        gh_bin.as_ref(),
        &[
            "run",
            "list",
            "--branch",
            "main",
            "--status",
            "completed",
            "--limit",
            &limit,
            "--json",
            "databaseId,conclusion,headSha,workflowName",
        ],
        cwd,
        "main_run_list",
        super::stopgate_read_timeout(),
    ) {
        Ok(out) => out,
        Err(error) => {
            super::log_bounded_read_error("main-head", &error);
            return None;
        }
    };
    if !list_out.status.success() {
        let error =
            super::GhReadError::failed("main_run_list", super::stderr_tail(&list_out.stderr_tail));
        super::log_bounded_read_error("main-head", &error);
        return None; // gh error -> unknown -> hold
    }
    let list: Value = match serde_json::from_slice(&list_out.stdout) {
        Ok(value) => value,
        Err(parse_error) => {
            let error = super::GhReadError::failed("main_run_list_parse", parse_error.to_string());
            super::log_bounded_read_error("main-head", &error);
            return None;
        }
    };
    let arr = match list.as_array() {
        Some(array) => array,
        None => {
            let error = super::GhReadError::parse_failed("main_run_list_shape");
            super::log_bounded_read_error("main-head", &error);
            return None;
        }
    };
    if arr.is_empty() {
        return None; // zero completed runs (new/quiet repo) is not proof -> unknown
    }
    let failing_run_ids = parse_failing_run_ids(&list);

    let mut names: Vec<String> = Vec::new();
    for id in failing_run_ids {
        let id_arg = id.to_string();
        let view_out = match super::bounded_read(
            gh_bin.as_ref(),
            &["run", "view", &id_arg, "--json", "jobs"],
            cwd,
            "main_run_view",
            super::stopgate_read_timeout(),
        ) {
            Ok(out) => out,
            Err(error) => {
                super::log_bounded_read_error("main-head", &error);
                return None;
            }
        };
        if !view_out.status.success() {
            let error = super::GhReadError::failed(
                "main_run_view",
                super::stderr_tail(&view_out.stderr_tail),
            );
            super::log_bounded_read_error("main-head", &error);
            return None; // any per-run gh error -> unknown -> hold (fail closed)
        }
        let view: Value = match serde_json::from_slice(&view_out.stdout) {
            Ok(value) => value,
            Err(parse_error) => {
                let error =
                    super::GhReadError::failed("main_run_view_parse", parse_error.to_string());
                super::log_bounded_read_error("main-head", &error);
                return None;
            }
        };
        for name in parse_failing_job_names(&view) {
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    Some(names)
}

/// Idempotency guard (Concurrency AC): true iff a prior `termination` event
/// with reason `DoneAwaitingMerge` for this session already exists, so a
/// re-evaluation (crash restart, or the two consumers racing) does not
/// double-emit or double-notify. Fail-open (false) on an unreadable events
/// file: at worst one extra notify, never a silent skip of the terminal.
pub(super) fn already_emitted_awaiting_merge(events_path: &Path, session_id: &str) -> bool {
    let content = crate::event_store::journal_text(events_path, &["termination"]);
    content.lines().any(|line| {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            return false;
        };
        val.get("type").and_then(|v| v.as_str()) == Some("termination")
            && val.pointer("/data/session_id").and_then(|v| v.as_str()) == Some(session_id)
            && val.pointer("/data/reason").and_then(|v| v.as_str()) == Some("DoneAwaitingMerge")
    })
}

/// The crown's ruling on a node, if any: `dispatch_hold_verdict`'s reason
/// when the hold is validly HELD. An invalid hold, an absent hold, or any
/// read error (unreadable graph, no such node) returns `None` so the caller
/// falls through to the main-red proof or blocks as today - a ruling is
/// proof only when it reads clean.
pub(super) fn ruling_hold(node_id: &str) -> Option<String> {
    ruling_hold_at(node_id, &crate::graph_get::default_graph_path())
}

fn ruling_hold_at(node_id: &str, graph_path: &Path) -> Option<String> {
    let entries = crate::backlog::api::rows(&crate::backlog::api::Store::new(graph_path)).ok()?;
    let entry = crate::graph_get::find_entry(&entries, node_id)?.clone();
    let by_id: std::collections::BTreeMap<String, Value> = entries
        .iter()
        .filter_map(|e| {
            e.get("id")
                .and_then(Value::as_str)
                .map(|id| (id.to_string(), e.clone()))
        })
        .collect();
    let verdict = crate::backlog_ready::dispatch_hold_verdict(&entry, &by_id)?;
    verdict.held.then_some(verdict.guard_reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_failing_run_ids_keeps_only_the_newest_verdict_per_workflow() {
        // cli-ci's newest run is still in progress (no conclusion terminal
        // enough - a "cancelled" or missing entry never appears in a
        // completed-only list, so here we simulate its newest COMPLETED run
        // trailing behind rust-ci's). rust-ci's newest completed run passed;
        // an older rust-ci failure must not be counted (superseded).
        let list = serde_json::json!([
            {"databaseId": 1, "conclusion": "success", "workflowName": "rust-ci"},
            {"databaseId": 2, "conclusion": "failure", "workflowName": "rust-ci"},
            {"databaseId": 3, "conclusion": "failure", "workflowName": "cli-ci"},
            {"databaseId": 4, "conclusion": "cancelled", "workflowName": "guards"},
            {"databaseId": 5, "conclusion": "failure", "workflowName": "guards"},
        ]);
        let mut got = parse_failing_run_ids(&list);
        got.sort();
        // rust-ci's newest (1, success) wins over the older failure (2).
        // cli-ci's newest completed run (3) is a failure -> counted.
        // guards' newest completed run (4) is cancelled (no verdict), so
        // its older failure (5) is consulted and counted.
        assert_eq!(got, vec![3, 5]);
    }

    #[test]
    fn parse_failing_run_ids_empty_on_malformed_or_unnamed() {
        assert!(parse_failing_run_ids(&serde_json::json!({})).is_empty());
        assert!(parse_failing_run_ids(&serde_json::json!([{"conclusion": "failure"}])).is_empty());
    }

    #[test]
    fn parse_failing_job_names_only_failed_jobs() {
        let view = serde_json::json!({
            "jobs": [
                {"name": "codex",   "conclusion": "success"},
                {"name": "cargo test + schema parity", "conclusion": "failure"},
                {"name": "gemini",  "conclusion": "failure"},
            ]
        });
        let mut got = parse_failing_job_names(&view);
        got.sort();
        assert_eq!(
            got,
            vec![
                "cargo test + schema parity".to_string(),
                "gemini".to_string()
            ]
        );
        // No jobs key -> empty, never panics.
        assert!(parse_failing_job_names(&serde_json::json!({})).is_empty());
    }

    /// AC1-HP: the core shape - PR fails only the one check main also fails.
    #[test]
    fn subset_rule_pr_failing_is_covered_by_main() {
        let pr = vec!["cargo test + schema parity".to_string()];
        let main = vec![
            "cargo test + schema parity".to_string(),
            "some other main-only red".to_string(),
        ];
        assert!(is_pre_existing_main_red(&pr, &main));
    }

    /// AC1-EDGE: a PR-unique failing check (its own breakage) blocks the terminal.
    #[test]
    fn subset_rule_pr_unique_red_blocks() {
        let pr = vec![
            "cargo test + schema parity".to_string(),
            "fmt gate".to_string(), // the session's own breakage
        ];
        let main = vec!["cargo test + schema parity".to_string()];
        assert!(!is_pre_existing_main_red(&pr, &main));
    }

    #[test]
    fn subset_rule_empty_pr_failing_never_eligible() {
        // Empty PR-failing is the DonePRGreen path, not this one.
        assert!(!is_pre_existing_main_red(&[], &["x".to_string()]));
        // Non-empty PR vs green main (empty) -> hold.
        assert!(!is_pre_existing_main_red(&["x".to_string()], &[]));
    }

    #[test]
    fn already_emitted_awaiting_merge_detects_prior_and_absence() {
        let dir = tempfile::tempdir().unwrap();
        let events = dir.path().join("events.jsonl");
        // Absent file -> false (fail open).
        assert!(!already_emitted_awaiting_merge(&events, "sess-A"));
        // A DonePRGreen termination for the same session must NOT count.
        std::fs::write(
            &events,
            "{\"type\":\"termination\",\"data\":{\"session_id\":\"sess-A\",\"reason\":\"DonePRGreen\"}}\n",
        )
        .unwrap();
        assert!(!already_emitted_awaiting_merge(&events, "sess-A"));
        // A prior DoneAwaitingMerge for sess-A counts; a different session does not.
        std::fs::write(
            &events,
            "{\"type\":\"termination\",\"data\":{\"session_id\":\"sess-A\",\"reason\":\"DoneAwaitingMerge\"}}\n",
        )
        .unwrap();
        assert!(already_emitted_awaiting_merge(&events, "sess-A"));
        assert!(!already_emitted_awaiting_merge(&events, "sess-B"));
    }

    fn write_hold_plan(dir: &std::path::Path, hold_frontmatter: &str) -> String {
        let plan = dir.join("plan.md");
        std::fs::write(
            &plan,
            format!("---\nstatus: ready\n{hold_frontmatter}---\n\n# Held\n"),
        )
        .unwrap();
        plan.display().to_string()
    }

    fn write_graph(dir: &std::path::Path, name: &str, entry: Value) -> std::path::PathBuf {
        // One store per fixture: a second seed into the same db would trip
        // the publish read-back (the store already holds the first row).
        let graph = dir.join(name);
        crate::graph_store::seed_rows(&graph, &[entry]).unwrap();
        graph
    }

    /// AC-shared: `ruling_hold_at` names the guard_reason for a valid hold,
    /// and stays None for an invalid one - the two dispositions the
    /// loop-check arm branches on.
    #[test]
    fn ruling_hold_reads_a_valid_block_and_refuses_an_invalid_one() {
        let dir = tempfile::tempdir().unwrap();
        let plan_path = write_hold_plan(
            dir.path(),
            "dispatch_hold:\n  reason: waiting on legal\n  release_when: legal clears\n  review_on: 2099-01-01\n  set_by: operator\n",
        );
        let graph = write_graph(
            dir.path(),
            "graph-held.json",
            serde_json::json!({"id": "x-held", "plan_path": plan_path}),
        );
        assert_eq!(
            ruling_hold_at("x-held", &graph),
            Some("dispatch-hold:x-held".to_string())
        );

        let invalid_plan =
            write_hold_plan(dir.path(), "dispatch_hold:\n  reason: waiting on legal\n");
        let graph2 = write_graph(
            dir.path(),
            "graph-broken.json",
            serde_json::json!({"id": "x-broken", "plan_path": invalid_plan}),
        );
        assert_eq!(ruling_hold_at("x-broken", &graph2), None);
    }

    /// AC2-IDEM: a store-committed DoneAwaitingMerge satisfies the guard with
    /// no raw bytes anywhere.
    #[test]
    fn already_emitted_reads_a_store_committed_termination() {
        let dir = tempfile::tempdir().unwrap();
        let events = dir.path().join("events.jsonl");
        let line = serde_json::json!({
            "ts": "2026-06-06T00:00:00Z", "type": "termination", "source": "hook",
            "data": {"session_id": "sess-a", "reason": "DoneAwaitingMerge"}
        })
        .to_string();
        crate::event_store::append_envelope(&events, &line, None).unwrap();
        assert!(already_emitted_awaiting_merge(&events, "sess-a"));
        assert!(!already_emitted_awaiting_merge(&events, "sess-b"));
    }
}
