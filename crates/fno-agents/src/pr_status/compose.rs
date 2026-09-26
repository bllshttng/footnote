//! The status payload composer: one JSON payload, one stderr note stream,
//! one exit code, exactly the Python `run_status` contract the goldens pin.
//!
//! The pure half (`compose_payload` + the renderers) takes the facts this
//! read already holds and answers byte-for-byte against
//! crates/fno-agents/tests/fixtures/pr_status/. The I/O half (live seams for
//! review coverage, optional reviews, the preview receipt) lands with the
//! door; the golden contract never sees it.

use super::{has_settled_marker, verdict_for, without_coverage_statuses};
use serde_json::{json, Map, Value};

/// The pure composer: every input arrives resolved, the payload and its
/// stderr notes leave composed. `inputs` mirrors the fixture schema's
/// `inputs` object so the replay feeds one shape.
pub(crate) struct ComposeInputs {
    pub pr: String,
    pub pr_json: Value,
    pub rerun_recovery: Value,
    pub branch_history: Value,
    pub optional_reviews: Value,
    pub coverage_row: Value,
    pub hold_reason: Value,
    pub review_activity: Value,
    pub receipt: Value,
    pub github_merge_blockers: Value,
    pub merge_authority: Value,
    pub merge_execution: Value,
    pub failures: Value,
    pub review_lane: bool,
}

/// One status read: the exit code, the stdout JSON payload, the stderr lines
/// in print order. Exit is always the CI verdict's code.
pub(crate) fn compose_payload(inputs: &ComposeInputs) -> (i32, Value, Vec<String>) {
    let rollup = inputs
        .pr_json
        .get("statusCheckRollup")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let generic_rollup = without_coverage_statuses(&rollup);
    let (verdict, code, counts) = verdict_for(&generic_rollup);
    let green = verdict == "green";
    let is_terminal = matches!(
        inputs
            .pr_json
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_uppercase()
            .as_str(),
        "MERGED" | "CLOSED"
    );

    let prior_failures_known = |payload: &Value| -> Map<String, Value> {
        let mut known = Map::new();
        if let Some(failures) = payload.get("failures").and_then(Value::as_array) {
            for f in failures {
                if let Some(id) = f.get("job_id").and_then(Value::as_str) {
                    known.insert(id.to_string(), f.clone());
                }
            }
        }
        known
    };
    let _ = prior_failures_known; // used by the live half; the golden replay injects `failures`

    // Failure detail arrives resolved (the live half collects it before this
    // point so a red read carries detail in the row the cache serves).
    let failures = inputs.failures.clone();

    // A terminal PR skips the review probes and prints the no-pending
    // answers, never `unknown`: nothing failed, it was deliberately not asked.
    let (reviews_unresolved, reviews_resolved_unchanged, reviews_list) = if is_terminal {
        (json!(0), json!(0), json!([]))
    } else {
        (
            inputs
                .optional_reviews
                .get("optional_reviews_unresolved")
                .cloned()
                .unwrap_or(Value::Null),
            inputs
                .optional_reviews
                .get("optional_reviews_resolved_unchanged")
                .cloned()
                .unwrap_or(Value::Null),
            inputs
                .optional_reviews
                .get("optional_reviews")
                .cloned()
                .unwrap_or(json!("unknown")),
        )
    };
    let coverage: Value = if is_terminal {
        // Deliberate skip, spelled with the full NOT_ASKED shape: a reader
        // must tell "nobody looked because terminal" from "the probe died".
        json!({
            "coverage": "not_asked",
            "reviewed_count": 0,
            "self_attested_count": 0,
            "head_sha": null,
            "stale_verdicts": [],
            "note": "not asked: PR is terminal (merged or closed); this says nothing about coverage at merge time",
        })
    } else {
        inputs.coverage_row.clone()
    };
    let activity = if is_terminal {
        json!({
            "blocker": "",
            "detail": "",
            "hold": null,
            "worktree": {
                "probed": false,
                "path": null,
                "dirty": null,
                "head": null,
                "note": "not asked: PR is terminal",
            },
        })
    } else {
        inputs.review_activity.clone()
    };
    let review_lane = if is_terminal {
        false
    } else {
        inputs.review_lane
    };

    // Rerun recovery rides green reads of live PRs; a prior green at the same
    // head with the same check total replays instead of re-probing.
    let head_sha = inputs
        .pr_json
        .get("headRefOid")
        .cloned()
        .unwrap_or(Value::Null);
    let mut rerun_fields = Map::new();
    if green && !is_terminal {
        if let Some(recovered) = inputs
            .rerun_recovery
            .get("recovered")
            .and_then(Value::as_bool)
        {
            rerun_fields.insert("rerun_recovered".into(), json!(recovered));
            rerun_fields.insert(
                "recovered_failures".into(),
                inputs
                    .rerun_recovery
                    .get("failed")
                    .cloned()
                    .unwrap_or(json!([])),
            );
        }
    }

    // ONE merge decision: the preview receipt rides the ask, so the owner
    // never spawns a second status read.
    let receipt = inputs.receipt.clone();
    let blocker_words: Vec<String> = receipt
        .get("blockers")
        .and_then(Value::as_array)
        .map(|blockers| {
            blockers
                .iter()
                .filter_map(|b| b.get("code").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    let mut payload = Map::new();
    payload.insert("pr".into(), json!(inputs.pr));
    payload.insert("head".into(), head_sha);
    payload.insert("verdict".into(), json!(verdict));
    let unsettled = counts.get("unsettled").and_then(Value::as_i64).unwrap_or(0);
    let total = counts.get("total").and_then(Value::as_i64).unwrap_or(0);
    payload.insert(
        "settled".into(),
        json!(verdict != "unknown" && total > 0 && unsettled == 0),
    );
    payload.insert("green".into(), json!(green));
    payload.insert(
        "pr_state".into(),
        inputs.pr_json.get("state").cloned().unwrap_or(Value::Null),
    );
    payload.insert(
        "mergeable".into(),
        inputs
            .pr_json
            .get("mergeable")
            .cloned()
            .unwrap_or(Value::Null),
    );
    payload.insert(
        "github_merge_state".into(),
        inputs.github_merge_blockers.clone(),
    );
    payload.insert("checks".into(), counts);
    if !inputs.branch_history.is_null() {
        payload.insert("branch_history".into(), inputs.branch_history.clone());
    }
    if !failures.is_null() {
        payload.insert("failures".into(), failures.clone());
    }
    for (k, v) in &rerun_fields {
        payload.insert(k.clone(), v.clone());
    }
    payload.insert("optional_reviews".into(), reviews_list);
    payload.insert(
        "optional_reviews_unresolved".into(),
        reviews_unresolved.clone(),
    );
    payload.insert(
        "optional_reviews_resolved_unchanged".into(),
        reviews_resolved_unchanged.clone(),
    );
    payload.insert("review_coverage".into(), coverage.clone());
    payload.insert(
        "review_posture".into(),
        coverage
            .get("review_posture")
            .cloned()
            .unwrap_or(Value::Null),
    );
    payload.insert("merge_authority".into(), inputs.merge_authority.clone());
    payload.insert(
        "merge_execution".into(),
        if is_terminal {
            Value::Null
        } else {
            inputs.merge_execution.clone()
        },
    );
    if let Some(rounds_used) = coverage.get("rounds_used") {
        if !rounds_used.is_null() {
            payload.insert("rounds_used".into(), rounds_used.clone());
            payload.insert(
                "max_rounds".into(),
                coverage.get("rounds_max").cloned().unwrap_or(Value::Null),
            );
            payload.insert(
                "rounds_exhausted".into(),
                coverage
                    .get("rounds_exhausted")
                    .cloned()
                    .unwrap_or(Value::Null),
            );
        } else {
            payload.insert("rounds_used".into(), Value::Null);
            payload.insert("max_rounds".into(), Value::Null);
            payload.insert("rounds_exhausted".into(), Value::Null);
            payload.insert(
                "rounds_note".into(),
                json!("no review_coverage row at this head; run fno-agents review-coverage"),
            );
        }
    } else {
        payload.insert("rounds_used".into(), Value::Null);
        payload.insert("max_rounds".into(), Value::Null);
        payload.insert("rounds_exhausted".into(), Value::Null);
        payload.insert(
            "rounds_note".into(),
            json!("no review_coverage row at this head; run fno-agents review-coverage"),
        );
    }
    payload.insert("review_activity".into(), activity);
    payload.insert("dispatch_hold".into(), inputs.hold_reason.clone());
    payload.insert("merge_decision".into(), receipt.clone());
    payload.insert("ready".into(), json!(blocker_words.is_empty()));
    payload.insert("ready_blockers".into(), json!(blocker_words));
    if let Some(waiver) = receipt.get("coverage_waiver") {
        if !waiver.is_null() {
            payload.insert("coverage_waiver".into(), waiver.clone());
        }
    }

    let mut stderr: Vec<String> = Vec::new();
    stderr.push(verdict_line(&Value::Object(payload.clone())));
    // The notes, in the Python leg's print order.
    rerun_recovery_note(&payload, &mut stderr);
    let _ = review_lane; // coverage-status repost is a live-half write, never a golden arm
    push_unsettled_notes(&verdict, &generic_rollup, &mut stderr);
    push_review_notes(
        &reviews_unresolved,
        &reviews_resolved_unchanged,
        &mut stderr,
    );
    push_coverage_notes(&coverage, &payload, &mut stderr);
    failures_note(&payload, &mut stderr);
    (code, Value::Object(payload), stderr)
}

/// One human line for the payload; the fleet greps stderr for
/// `"settled": true` on this verb, so stdout stays the machine contract.
pub(crate) fn verdict_line(payload: &Value) -> String {
    let checks = payload.get("checks").cloned().unwrap_or(json!({}));
    let unsettled = checks.get("unsettled").and_then(Value::as_i64);
    let settled_slot = if payload.get("settled").and_then(Value::as_bool) == Some(true) {
        "settled".to_string()
    } else {
        match unsettled {
            Some(n) => format!("unsettled({n})"),
            None => "unsettled".to_string(),
        }
    };
    let mergeable_slot = match payload.get("mergeable").and_then(Value::as_str) {
        Some("MERGEABLE") => "mergeable".to_string(),
        Some("CONFLICTING") => "CONFLICTING".to_string(),
        Some("UNKNOWN") => "mergeable-unknown(not-yet-computed)".to_string(),
        _ => "mergeable-unavailable(no-answer)".to_string(),
    };
    let head = payload.get("head").and_then(Value::as_str).unwrap_or("");
    let head = if head.is_empty() { "unknown" } else { head };
    let coverage = payload.get("review_coverage").cloned().unwrap_or(json!({}));
    let cov_head = coverage
        .get("head_sha")
        .and_then(Value::as_str)
        .unwrap_or("");
    let coverage_at = if !cov_head.is_empty() && cov_head != head {
        format!(" (coverage at {})", &cov_head[..cov_head.len().min(12)])
    } else {
        String::new()
    };
    let mut blockers: Vec<String> = payload
        .get("ready_blockers")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|b| b.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if let Some(stale) = payload.get("stale_reason").and_then(Value::as_str) {
        blockers.push(format!("stale_serve: {stale}"));
    }
    if payload.get("verdict").and_then(Value::as_str) == Some("error") {
        if let Some(reason) = payload.get("reason").and_then(Value::as_str) {
            blockers.push(format!("error: {reason}"));
        }
    }
    let clause = if blockers.is_empty() {
        "no blockers".to_string()
    } else {
        format!("{} blockers: {}", blockers.len(), blockers.join(", "))
    };
    let missing = payload
        .pointer("/github_merge_state/missing_required_checks")
        .and_then(Value::as_array)
        .map(|a| {
            let names: Vec<String> = a
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            format!(" (missing: {})", names.join(", "))
        })
        .unwrap_or_default();
    let mut fail_slot = String::new();
    if let Some(failures) = payload.get("failures").and_then(Value::as_array) {
        if let Some(first) = failures.first() {
            let mut label = first
                .get("check")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string();
            if let Some(step) = first.get("step").and_then(Value::as_str) {
                label.push_str(&format!("[{step}]"));
            }
            fail_slot = format!(" failing: {label}");
        }
    }
    let history_slot = payload
        .pointer("/branch_history/line")
        .and_then(Value::as_str)
        .map(|line| format!(" history: {line}"))
        .unwrap_or_default();
    let pr_slot = payload
        .get("pr")
        .map(|v| v.to_string().trim_matches('"').to_string())
        .unwrap_or_else(|| "UNKNOWN".into());
    let state = payload
        .get("pr_state")
        .and_then(Value::as_str)
        .unwrap_or("UNKNOWN")
        .to_uppercase();
    let verdict = payload
        .get("verdict")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let head12 = &head[..head.len().min(12)];
    let ready = if payload.get("ready").and_then(Value::as_bool) == Some(true) {
        "ready"
    } else {
        "NOT-ready"
    };
    let clause_full = format!("{clause}{missing}");
    format!(
        "{pr_slot} {state} {verdict} {settled_slot} {mergeable_slot} {ready} @ {head12}{coverage_at}{history_slot} - {clause_full}{fail_slot}"
    )
}

fn rerun_recovery_note(payload: &Map<String, Value>, out: &mut Vec<String>) {
    if payload.get("rerun_recovered").and_then(Value::as_bool) == Some(true) {
        let names: Vec<String> = payload
            .get("recovered_failures")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|v| v.as_str().unwrap_or("unknown").to_string())
                    .collect()
            })
            .unwrap_or_else(|| vec!["unknown".into()]);
        let names = if names.is_empty() {
            "unknown".to_string()
        } else {
            names.join(", ")
        };
        out.push(format!(
            "note: green on re-run; earlier failed attempt: {names}. A passing re-run is a recovery, not proof the defect is gone."
        ));
    }
}

fn push_unsettled_notes(verdict: &str, generic_rollup: &[Value], out: &mut Vec<String>) {
    let unsettled: Vec<&Value> = generic_rollup
        .iter()
        .filter(|c| !has_settled_marker(c))
        .collect();
    if unsettled.is_empty() {
        return;
    }
    let absent: Vec<&Value> = unsettled
        .iter()
        .copied()
        .filter(|c| {
            let status = c
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_uppercase();
            status.is_empty() || status == "COMPLETED"
        })
        .collect();
    let running: Vec<&&Value> = unsettled.iter().filter(|c| !absent.contains(c)).collect();
    if !absent.is_empty() {
        let names: Vec<String> = absent.iter().map(|c| entry_name(c)).collect();
        out.push(format!(
            "note: {} check(s) produced no result (cancelled or stale): {}. The verdict is {verdict}, and settled stays false because a cancelled run is an ABSENT result, not a terminal one. Push again or rerun the workflow. Do not read this PR as decided.",
            absent.len(),
            names.join(", ")
        ));
    }
    if !running.is_empty() {
        let names: Vec<String> = running.iter().map(|c| entry_name(c)).collect();
        out.push(format!(
            "note: {} check(s) are still queued or running: {}. The verdict is {verdict}, and settled stays false until every latest run finishes. Wait for the run to finish. Do not start a new one.",
            running.len(),
            names.join(", ")
        ));
    }
}

fn entry_name(c: &Value) -> String {
    let name = c.get("name").and_then(Value::as_str).unwrap_or("");
    if !name.is_empty() {
        return name.to_string();
    }
    let context = c.get("context").and_then(Value::as_str).unwrap_or("");
    if !context.is_empty() {
        return context.to_string();
    }
    "?".to_string()
}

fn push_review_notes(unresolved: &Value, resolved_unchanged: &Value, out: &mut Vec<String>) {
    if let Some(n) = unresolved.as_i64() {
        if n > 0 {
            out.push(format!(
                "note: {n} optional review finding(s) unresolved, so ready stays false. A REPLY DOES NOT RESOLVE A THREAD. Fix each one, or answer it in-thread, then resolve the thread explicitly: the \"Resolve conversation\" button, or `gh api graphql -f query='mutation($t: ID!){{resolveReviewThread(input:{{threadId: $t}}){{thread{{isResolved}}}}}}' -F t=<threadId>` (thread ids come from `reviewThreads` on the pullRequest)."
            ));
        }
    }
    if let Some(n) = resolved_unchanged.as_i64() {
        if n > 0 {
            out.push(format!(
                "note: {n} optional review thread(s) resolved while the original diff line remains current; this does not block ready. Verify the explicit resolution rationale before merge."
            ));
        }
    }
}

fn push_coverage_notes(coverage: &Value, payload: &Map<String, Value>, out: &mut Vec<String>) {
    if let Some(note) = coverage.get("recompute").and_then(Value::as_str) {
        if note != "recomputed" {
            out.push(format!("note: coverage recompute: {note}"));
        }
    }
    let cov_head = coverage
        .get("head_sha")
        .and_then(Value::as_str)
        .unwrap_or("");
    let stale = coverage.get("stale_verdicts").and_then(Value::as_array);
    if !cov_head.is_empty() || stale.map(|s| !s.is_empty()).unwrap_or(false) {
        let word = coverage
            .get("coverage")
            .and_then(Value::as_str)
            .unwrap_or("");
        let mut line = format!("note: review coverage {word}");
        if let Some(count) = coverage.get("reviewed_count") {
            if !count.is_null() {
                line.push_str(&format!(" ({count} reviewed"));
                if let Some(passed) = coverage.get("passed_count") {
                    if !passed.is_null() {
                        line.push_str(&format!(", {passed} passed"));
                    }
                }
                if let Some(self_n) = coverage.get("self_attested_count").and_then(Value::as_i64) {
                    if self_n > 0 {
                        line.push_str(&format!(", {self_n} self-attested"));
                    }
                }
                line.push(')');
            }
        }
        if !cov_head.is_empty() {
            line.push_str(&format!(
                " computed at {}",
                &cov_head[..cov_head.len().min(8)]
            ));
        }
        out.push(line);
    }
    if let Some(rows) = stale {
        for v in rows {
            let name = v.get("name").and_then(Value::as_str).unwrap_or("");
            let producer = v.get("producer").and_then(Value::as_str).unwrap_or("");
            let sha = v
                .get("reviewed_sha")
                .and_then(Value::as_str)
                .unwrap_or("an unknown commit");
            out.push(format!(
                "note: {name} ({producer}) reviewed {}, whose code no longer matches HEAD - that verdict does not count. Ask it to re-read.",
                &sha[..sha.len().min(8)]
            ));
        }
    }
    // The owner-guidance and reviewer-refused notes ride the live half; the
    // goldens pin the payload's `review_owner_guidance` field only.
    let _ = payload;
}

fn failures_note(payload: &Map<String, Value>, out: &mut Vec<String>) {
    let Some(failures) = payload.get("failures").and_then(Value::as_array) else {
        return;
    };
    for f in failures {
        let check = f
            .get("check")
            .and_then(Value::as_str)
            .unwrap_or("(unnamed check)");
        let mut line = format!("note: {check} failed");
        if let Some(step) = f.get("step").and_then(Value::as_str) {
            line.push_str(&format!(" at step '{step}'"));
        }
        if let Some(first_error) = f.get("first_error").and_then(Value::as_str) {
            line.push_str(&format!(": {first_error}"));
        }
        out.push(line);
        if let Some(unreached) = f.get("unreached_steps").and_then(Value::as_array) {
            let names: Vec<String> = unreached
                .iter()
                .map(|n| n.as_str().unwrap_or("").to_string())
                .collect();
            out.push(format!(
                "note: {check}: fail-fast never ran: {}. An unreached step is not a pass.",
                names.join(", ")
            ));
        }
        if let Some(detail) = f.get("detail").and_then(Value::as_str) {
            out.push(format!("note: {check}: {detail}"));
        }
    }
}

/// The refused read (exit 4): a loud error payload, never an absent answer.
/// `rate_limit_class` rides as a field, never as prose, because the cache
/// arms the fleet backoff on this value.
pub(crate) fn error_payload(pr: &str, reason: &super::RestReason) -> (i32, Value, Vec<String>) {
    let mut payload = Map::new();
    payload.insert("pr".into(), json!(pr));
    payload.insert("verdict".into(), json!("error"));
    payload.insert("settled".into(), json!(false));
    payload.insert("green".into(), json!(false));
    payload.insert("reason".into(), json!(reason.text));
    if !reason.rate_limit_class.is_empty() {
        payload.insert("rate_limit_class".into(), json!(reason.rate_limit_class));
    }
    let value = Value::Object(payload.clone());
    let stderr = vec![verdict_line(&value)];
    (4, value, stderr)
}

/// The stderr notes a cache serve replays, in the Python `_serve` order:
/// coverage recompute, failure detail, rerun recovery.
pub(crate) fn serve_notes(payload: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(coverage) = payload.get("review_coverage") {
        if let Some(note) = coverage.get("recompute").and_then(Value::as_str) {
            if note != "recomputed" {
                out.push(format!("note: coverage recompute: {note}"));
            }
        }
    }
    if let Some(obj) = payload.as_object() {
        failures_note(obj, &mut out);
        rerun_recovery_note(obj, &mut out);
    }
    out
}
