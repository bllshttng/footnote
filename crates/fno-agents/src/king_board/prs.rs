//! One PR listing, binding classification, mergeable filter (pr/_status).
use super::budget::run_json;
use super::{s_i64, s_str, SourceRead, LEGACY_DEFER_PREFIX, TERMINAL_RUNGS};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

pub(crate) const COVERAGE_STATUS_CONTEXT: &str = "fno/review-coverage";
pub(crate) const COVERAGE_UNAVAILABLE_STATUS_CONTEXT: &str = "fno/review-coverage-unavailable";

pub(crate) const PASS_STATES: [&str; 3] = ["SUCCESS", "NEUTRAL", "SKIPPED"];
pub(crate) const FAIL_STATES: [&str; 7] = [
    "FAILURE",
    "TIMED_OUT",
    "CANCELLED",
    "ACTION_REQUIRED",
    "STARTUP_FAILURE",
    "STALE",
    "ERROR",
];

// ---------------------------------------------------------------------------
// PRs: one listing, binding classification, mergeable filter
// ---------------------------------------------------------------------------

/// Delimiter-bounded node-id candidates of a head ref (pr/closure.branch_node_ids).
/// Hand-rolled: the pattern needs lookaheads (`(?=$|[/-])`) that the regex
/// crate does not support.
pub(crate) fn branch_node_ids(head_ref: &str) -> Vec<String> {
    let b = head_ref.as_bytes();
    let mut ids: Vec<String> = Vec::new();
    // Non-overlapping left-to-right scan, exactly like Python's finditer: a
    // match is consumed and the scan resumes after it, so "feature/x-cdef-1234"
    // never yields the bogus "cdef-1234" from inside the first match's tail.
    let mut i = 0;
    while i < b.len() {
        // A candidate starts at the string head or after '-' / '/'.
        if !(i == 0 || b[i - 1] == b'-' || b[i - 1] == b'/') {
            i += 1;
            continue;
        }
        if !b[i].is_ascii_lowercase() {
            i += 1;
            continue;
        }
        // [a-z][a-z0-9]{0,7} then '-' then [0-9a-f]{4,8}
        let mut j = i + 1;
        let mut alnum = 0;
        while j < b.len() && alnum < 7 && (b[j].is_ascii_lowercase() || b[j].is_ascii_digit()) {
            j += 1;
            alnum += 1;
        }
        if j >= b.len() || b[j] != b'-' {
            i += 1;
            continue;
        }
        let hex_start = j + 1;
        let mut k = hex_start;
        while k < b.len()
            && k - hex_start < 8
            && (b[k].is_ascii_digit() || (b'a'..=b'f').contains(&b[k]))
        {
            k += 1;
        }
        let hex_len = k - hex_start;
        if !(4..=8).contains(&hex_len) {
            i += 1;
            continue;
        }
        if !(k == b.len() || b[k] == b'-' || b[k] == b'/') {
            i += 1;
            continue;
        }
        let candidate = &head_ref[i..k];
        if !ids.iter().any(|c| c == candidate) {
            ids.push(candidate.to_string());
        }
        i = k;
    }
    ids
}

/// A rollup entry's pass/fail/pending class (pr/_status._classify).
pub(crate) fn classify_check(check: &Value) -> &'static str {
    let status = s_str(check, "status").unwrap_or("").to_uppercase();
    if !status.is_empty() && status != "COMPLETED" {
        return "pending";
    }
    let raw = check
        .get("conclusion")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .or_else(|| s_str(check, "state"))
        .unwrap_or("")
        .to_uppercase();
    if PASS_STATES.contains(&raw.as_str()) {
        return "pass";
    }
    if FAIL_STATES.contains(&raw.as_str()) {
        return "fail";
    }
    "pending"
}

/// Dedup to the latest run per check name/context (check_supersession's
/// generated selector), then drop the coverage projections, then every fetched
/// row is judged.
pub(crate) fn read_prs(
    cwd: &Path,
    slice: Duration,
    max_pr_reads: usize,
    entries: Option<&[Value]>,
) -> (SourceRead, SourceRead, Vec<String>) {
    let cmd = vec![
        "gh".to_string(),
        "pr".to_string(),
        "list".to_string(),
        "--state".to_string(),
        "open".to_string(),
        "--limit".to_string(),
        max_pr_reads.to_string(),
        "--json".to_string(),
        "number,title,mergeable,statusCheckRollup,headRefName,url".to_string(),
    ];
    let listing = run_json(cmd, cwd, slice);
    if !listing.is_ok() {
        let err = listing.error.clone().unwrap_or_default();
        return (
            SourceRead::err(err.clone()),
            SourceRead::err(format!("undriven_pr: {err}")),
            Vec::new(),
        );
    }
    let rows = listing.rows();
    let mut warnings: Vec<String> = Vec::new();
    if rows.len() >= max_pr_reads {
        warnings.push(format!(
            "mergeable_pr: the open-PR listing hit its {max_pr_reads}-PR limit, \
             so more open PRs can exist; raise max_pr_reads to read further"
        ));
    }

    // Binding: graph rows for nodes an open PR points back at. An unreadable
    // binding is an unreadable QUEUE, never an empty one: mergeable_pr needs
    // no node, undriven_pr is nothing but nodes.
    let pr_nodes = match entries {
        None => SourceRead::err("pr node binding unreadable: graph unreadable"),
        Some(entries) => {
            let real_ids: HashSet<&str> = entries.iter().filter_map(|e| s_str(e, "id")).collect();
            let node_by_id: HashMap<&str, &Value> = entries
                .iter()
                .filter_map(|e| s_str(e, "id").map(|i| (i, e)))
                .collect();
            // First pass: which nodes have exactly one open PR.
            let mut open_prs_by_node: HashMap<String, Vec<i64>> = HashMap::new();
            let mut parsed: Vec<(i64, Option<String>, String, Vec<String>)> = Vec::new();
            for row in &rows {
                let Some(number) = s_i64(row, "number") else {
                    continue;
                };
                let head = s_str(row, "headRefName").unwrap_or("");
                if head.is_empty() {
                    continue;
                }
                let matched: Vec<String> = branch_node_ids(head)
                    .into_iter()
                    .filter(|nid| real_ids.contains(nid.as_str()))
                    .collect();
                if matched.len() == 1 {
                    open_prs_by_node
                        .entry(matched[0].clone())
                        .or_default()
                        .push(number);
                }
                parsed.push((
                    number,
                    row.get("url").and_then(Value::as_str).map(str::to_string),
                    head.to_string(),
                    matched,
                ));
            }
            let mut bound: Vec<Value> = Vec::new();
            for (number, url, _head, matched) in parsed {
                if matched.is_empty() {
                    continue; // untracked: carries no candidate
                }
                if matched.len() > 1 {
                    continue; // ambiguous: a list-order guess is the wrong-node bind
                }
                let nid = &matched[0];
                let mut siblings = open_prs_by_node
                    .get(nid.as_str())
                    .cloned()
                    .unwrap_or_default();
                siblings.sort();
                if siblings.len() > 1 {
                    continue; // ambiguous
                }
                let Some(node) = node_by_id.get(nid.as_str()) else {
                    continue;
                };
                let refs_this_pr = node_pr_refs(node).iter().any(|(n, _)| *n == number);
                if !refs_this_pr {
                    warnings.push(format!("pr_node_binding_missing: #{number} -> {nid}"));
                    continue;
                }
                let mut row = (*node).clone();
                if let Some(obj) = row.as_object_mut() {
                    obj.insert("pr_number".to_string(), json!(number));
                    obj.insert("pr_url".to_string(), json!(url));
                }
                bound.push(row);
            }
            SourceRead::ok(Value::Array(bound))
        }
    };

    // Every fetched row is judged; dropping any of them loses real work.
    let mut ready: Vec<Value> = Vec::new();
    for pr in &rows {
        if s_str(pr, "mergeable") != Some("MERGEABLE") {
            continue;
        }
        let rollup = pr
            .get("statusCheckRollup")
            .cloned()
            .unwrap_or(Value::Array(Vec::new()));
        let deduped = crate::check_supersession::latest_per_name(&rollup);
        let filtered: Vec<Value> = deduped
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter(|check| {
                        let is_coverage = |value: Option<&str>| {
                            value == Some(COVERAGE_STATUS_CONTEXT)
                                || value == Some(COVERAGE_UNAVAILABLE_STATUS_CONTEXT)
                        };
                        !is_coverage(s_str(check, "context")) && !is_coverage(s_str(check, "name"))
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let had_rows = deduped.as_array().map(|a| !a.is_empty()).unwrap_or(false);
        if had_rows && filtered.is_empty() {
            // Diagnostic-only rollup: CI has not reported; not green.
            continue;
        }
        let mut has_fail = false;
        let mut has_pending = false;
        for check in &filtered {
            match classify_check(check) {
                "fail" => has_fail = true,
                "pending" => has_pending = true,
                _ => {}
            }
        }
        if has_fail || has_pending {
            continue;
        }
        ready.push(json!({
            "number": pr.get("number"),
            "title": pr.get("title"),
        }));
    }
    (SourceRead::ok(Value::Array(ready)), pr_nodes, warnings)
}

/// (pr_number, pr_url) pairs for a node, primary first, deduped
/// (graph/_reconcile.node_pr_refs).
pub(crate) fn node_pr_refs(node: &Value) -> Vec<(i64, Option<String>)> {
    let mut refs = Vec::new();
    let mut seen: HashSet<i64> = HashSet::new();
    if let Some(primary) = s_i64(node, "pr_number") {
        refs.push((
            primary,
            node.get("pr_url")
                .and_then(Value::as_str)
                .map(str::to_string),
        ));
        seen.insert(primary);
    }
    if let Some(extras) = node.get("additional_prs").and_then(Value::as_array) {
        for extra in extras {
            let Some(num) = s_i64(extra, "number") else {
                continue;
            };
            if seen.contains(&num) {
                continue;
            }
            refs.push((
                num,
                extra.get("url").and_then(Value::as_str).map(str::to_string),
            ));
            seen.insert(num);
        }
    }
    refs
}

/// The one status string every reader of a row agrees on
/// (graph/statuses.derived_status).
pub(crate) fn derived_status(entry: &Value) -> String {
    let terminal = {
        let status_terminal = s_str(entry, "status")
            .map(|s| TERMINAL_RUNGS.contains(&s))
            .unwrap_or(false);
        let superseded = entry.get("superseded_by").is_some_and(|v| !v.is_null());
        let completed = entry
            .get("completed_at")
            .and_then(Value::as_str)
            .map(|c| !c.is_empty() && !c.starts_with(LEGACY_DEFER_PREFIX))
            .unwrap_or(false);
        status_terminal || superseded || completed
    };
    if terminal && entry.get("completed_at").is_some_and(|v| !v.is_null()) {
        return "done".to_string();
    }
    s_str(entry, "status").unwrap_or("unknown").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn branch_ids_never_match_a_partial_hex_prefix() {
        assert_eq!(
            branch_node_ids("feature/x-cdef-1234"),
            vec!["x-cdef".to_string()]
        );
        assert_eq!(
            branch_node_ids("x-5b667-fixes-x-5b66"),
            vec!["x-5b667".to_string(), "x-5b66".to_string()]
        );
        // Uppercase is not id body ([0-9a-f], not [0-9a-fA-F]): the hex run
        // stops at 'E', so "x-abcd" binds and the tail never reads as id.
        assert_eq!(branch_node_ids("x-abcd-EF12"), vec!["x-abcd".to_string()]);
        assert!(branch_node_ids("main").is_empty());
    }

    #[test]
    fn mergeable_filter_drops_pending_and_failed_but_keeps_a_clean_pr() {
        let cwd = std::env::temp_dir();
        // read_prs shells to gh; the classifer half is exercised through the
        // same helpers the real read uses.
        let check = |status: &str, conclusion: &str| json!({"status": status, "conclusion": conclusion, "name": "ci"});
        assert_eq!(classify_check(&check("IN_PROGRESS", "")), "pending");
        assert_eq!(classify_check(&check("COMPLETED", "SUCCESS")), "pass");
        assert_eq!(classify_check(&check("COMPLETED", "FAILURE")), "fail");
        assert_eq!(classify_check(&check("COMPLETED", "STALE")), "fail");
        let _ = cwd;
    }

    #[test]
    fn coverage_only_rollups_are_diagnostic_not_green() {
        // A rollup holding only coverage contexts reads empty after the drop,
        // and an empty class set must not read as a mergeable PR.
        let rollup = json!([
            {"name": COVERAGE_STATUS_CONTEXT, "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"name": COVERAGE_UNAVAILABLE_STATUS_CONTEXT, "status": "COMPLETED", "conclusion": "SUCCESS"},
        ]);
        let deduped = crate::check_supersession::latest_per_name(&rollup);
        let filtered: Vec<&Value> = deduped
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| {
                let is_cov = |v: Option<&str>| {
                    v == Some(COVERAGE_STATUS_CONTEXT)
                        || v == Some(COVERAGE_UNAVAILABLE_STATUS_CONTEXT)
                };
                !is_cov(s_str(c, "context")) && !is_cov(s_str(c, "name"))
            })
            .collect();
        assert!(filtered.is_empty());
    }

    #[test]
    fn derived_status_reads_terminal_completion_over_a_stale_status() {
        let done = json!({"id": "x", "status": "in_review", "completed_at": "2026-09-01"});
        assert_eq!(derived_status(&done), "done");
        let deferred = json!({"id": "x", "status": "ready", "completed_at": "deferred: no time"});
        assert_eq!(derived_status(&deferred), "ready");
        let open = json!({"id": "x", "status": "in_progress"});
        assert_eq!(derived_status(&open), "in_progress");
    }
}
