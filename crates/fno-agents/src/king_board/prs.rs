//! One PR listing, binding classification, mergeable filter (pr/_status).
use super::budget::{fno_py_cmd, run_json, run_with_timeout};
use super::queues::NODE_ID_BODY;
use super::{is_terminal, s_i64, s_str, SourceRead};
use crate::graph_store::entry_id;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

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
    // match is consumed and the scan resumes after it, so "feature/x-aaaa-1234"
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

/// One failed `gh pr list` read rendered as both queue reads it feeds.
/// Sits between run_json and the queues so the listing's verdict survives
/// the split: a budget kill must not become "unreadable" here.
fn split_failed_listing(listing: &SourceRead) -> (SourceRead, SourceRead) {
    let err = listing.error.clone().unwrap_or_default();
    (
        listing.rewrap(err.clone()),
        listing.rewrap(format!("undriven_pr: {err}")),
    )
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
        "number,title,mergeable,statusCheckRollup,headRefName,url,body".to_string(),
    ];
    let listing = run_json(cmd, cwd, slice);
    if !listing.is_ok() {
        let (mergeable, undriven) = split_failed_listing(&listing);
        return (mergeable, undriven, Vec::new());
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
            let (bound, bind_warnings) = classify_pr_bindings(&rows, entries);
            warnings.extend(bind_warnings);
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

/// One candidate's merge-gate verdict: the payload `fno do pr status` prints
/// as JSON on stdout. Exits 0-3 are CI verdicts and carry that payload, so
/// they are read; exit 4 and every other exit is an unanswered gate - the
/// reader failed, and no trustworthy `ready` exists to extract. The runner
/// keeps only the error text, so a PR that went red between the listing and
/// the gate reads `ready: false` with `ci_red` in `ready_blockers` (a real
/// verdict, still not mergeable), while a crashed gate read renders
/// not-actionable with a warning naming the exit.
fn read_pr_gate(cwd: &Path, number: i64, timeout: Duration) -> Result<Value, String> {
    let mut cmd = fno_py_cmd();
    cmd.extend([
        "do".to_string(),
        "pr".to_string(),
        "status".to_string(),
        number.to_string(),
    ]);
    match run_with_timeout_accepting(&cmd, cwd, timeout, &[0, 1, 2, 3]).map(|o| o.stdout) {
        Err(f) => Err(f.message().to_string()),
        Ok(stdout) => serde_json::from_slice::<Value>(&stdout)
            .map_err(|e| format!("unparseable status payload: {e}")),
    }
}

/// Ask the merge gate about every candidate the listing called green
/// (: the queue's only evidence was that the PR is open; a live review
/// hold or an uncovered head made it unfusable and nothing said so). The
/// candidates read four at a time under ONE budgeted slice; a PR whose gate
/// call fails or whose slice runs out is simply absent from the answer, and
/// its warning names it - build renders an absent verdict as not-actionable,
/// never as mergeable.
pub(crate) fn read_pr_gates(
    cwd: &Path,
    numbers: &[i64],
    slice: Option<Duration>,
) -> (SourceRead, Vec<String>) {
    let Some(slice) = slice else {
        return (
            SourceRead::err("merge gate not read: board budget exhausted before the source"),
            Vec::new(),
        );
    };
    // Bounded fan-out: one fno-py cold start per candidate is the price of
    // asking the gate, but every candidate at once would spend the fleet's
    // shared gh quota faster than any slice can police.
    const GATE_FANOUT: usize = 4;
    let deadline = Instant::now() + slice;
    let mut rows: Vec<Value> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    for chunk in numbers.chunks(GATE_FANOUT) {
        // Spawn the whole chunk, then join: every member of the chunk runs
        // concurrently, and a panicked reader lands as Err, never unwinds.
        let answers: Vec<(i64, Option<Result<Value, String>>)> = std::thread::scope(|s| {
            let mut pending: Vec<(
                i64,
                Option<std::thread::ScopedJoinHandle<Result<Value, String>>>,
            )> = Vec::new();
            for n in chunk {
                let left = deadline.saturating_duration_since(Instant::now());
                let cwd = cwd.to_path_buf();
                if left.is_zero() {
                    pending.push((*n, None));
                } else {
                    let h = s.spawn(move || read_pr_gate(&cwd, *n, left));
                    pending.push((*n, Some(h)));
                }
            }
            pending
                .into_iter()
                .map(|(n, h)| {
                    let answer = h.map(|h| {
                        h.join()
                            .unwrap_or_else(|_| Err("gate reader panicked".to_string()))
                    });
                    (n, answer)
                })
                .collect()
        });
        for (n, answer) in answers {
            match answer {
                None => skipped.push(n.to_string()),
                Some(Ok(payload)) => match payload.get("ready").and_then(Value::as_bool) {
                    Some(ready) => rows.push(json!({
                        "number": n,
                        "ready": ready,
                        "ready_blockers": payload
                            .get("ready_blockers")
                            .cloned()
                            .unwrap_or(Value::Array(Vec::new())),
                    })),
                    None => warnings.push(format!(
                        "merge gate answered no verdict for PR {n}: {}",
                        s_str(&payload, "reason").unwrap_or("no ready field")
                    )),
                },
                Some(Err(e)) => warnings.push(format!("merge gate unreadable for PR {n}: {e}")),
            }
        }
    }
    if !skipped.is_empty() {
        warnings.push(format!(
            "merge gate read stopped at its slice; PR(s) {} unanswered",
            skipped.join(", ")
        ));
    }
    // The merge slot is the fact that ORDERS this queue: name its
    // holder on every row, and when the holder's own row is absent (the
    // listing filter or a spent gate slice dropped it), carry a synthetic
    // row so the queue names the PR every queued merge waits behind. One
    // store: the space db the claim verb reads with no root.
    match crate::claim_store::list_db(Some("merge-slot:"), false, None) {
        Ok(rows_json) => {
            // list_db answers {"rows": [...]}; each row carries the claim's
            // key and holder, so the stamp names WHICH base's slot it is.
            let slots: Vec<(u64, String)> = rows_json
                .get("rows")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(|r| {
                            let holder = r.get("holder").and_then(Value::as_str)?;
                            let pr = crate::authorized_merge::parse_slot_holder(holder)?;
                            let key = r.get("key").and_then(Value::as_str).unwrap_or("");
                            let base = key.strip_prefix("merge-slot:").unwrap_or(key);
                            Some((pr, base.to_string()))
                        })
                        .collect()
                })
                .unwrap_or_default();
            for (holder, base) in &slots {
                for row in rows.iter_mut() {
                    row.as_object_mut().map(|o| {
                        o.insert(
                            "merge_slot".to_string(),
                            json!({ "holder": holder, "base": base }),
                        )
                    });
                }
                let named = rows
                    .iter()
                    .any(|row| row.get("number").and_then(Value::as_u64) == Some(*holder));
                if !named {
                    rows.push(json!({
                        "number": holder,
                        "merge_slot_holder": true,
                    }));
                    warnings.push(format!(
                        "mergeable_pr: merge slot holder PR {holder} has no gate row in \
                         this read; carried as a slot row so the queue names what orders it"
                    ));
                }
            }
        }
        Err(e) => warnings.push(format!("mergeable_pr: merge slot unreadable: {e}")),
    }
    (SourceRead::ok(Value::Array(rows)), warnings)
}

/// A PR URL reduced to its comparable form: whitespace trimmed, query and
/// fragment dropped, trailing slash dropped, lowercased (mirrors
/// _reconcile._normalized_pr_url). An empty result means "no comparable URL".
fn normalized_pr_url(url: &str) -> String {
    url.trim()
        .split(|c| c == '?' || c == '#')
        .next()
        .unwrap_or("")
        .trim_end_matches('/')
        .to_lowercase()
}

/// Well-formed node ids named on the LAST exact `Backlog-Closure:` line of a
/// body, order-preserved, deduplicated (mirrors closure.parse_closure_trailer).
/// Case-insensitive key, token split on whitespace and commas, malformed
/// tokens dropped.
fn trailer_node_ids(body: &str) -> Vec<String> {
    let id_re = regex::Regex::new(&format!("^{NODE_ID_BODY}$")).expect("static regex");
    let mut last: Option<&str> = None;
    for line in body.lines() {
        let is_trailer = line
            .get(..16)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("Backlog-Closure:"));
        if is_trailer {
            last = Some(&line[16..]);
        }
    }
    let Some(rest) = last else {
        return Vec::new();
    };
    let mut ids: Vec<String> = Vec::new();
    for token in rest.split(|c: char| c == ',' || c.is_whitespace()) {
        if !token.is_empty() && id_re.is_match(token) && !ids.iter().any(|i| i == token) {
            ids.push(token.to_string());
        }
    }
    ids
}

/// The three binding keys of one PR, filtered to real graph ids. Shared by
/// the board classifier and the merge owner (`authorized_merge`), so the
/// board's untracked warning and a merge refusal cannot disagree about the
/// same PR.
pub(crate) struct PrBinding {
    /// Delimiter-bounded node ids the head ref names (`branch_node_ids`).
    pub branch: Vec<String>,
    /// Node ids whose `(pr_number, pr_url)` back-pointer names this PR,
    /// scoped by normalized URL because a pr_number is only unique within
    /// one repository. Sorted.
    pub backrefs: Vec<String>,
    /// Node ids on the LAST exact `Backlog-Closure:` body line
    /// (`trailer_node_ids`).
    pub trailer: Vec<String>,
}

impl PrBinding {
    /// The unbound detail when all three keys miss, else `None`.
    pub(crate) fn unbound_detail(&self) -> Option<String> {
        if self.branch.is_empty() && self.backrefs.is_empty() && self.trailer.is_empty() {
            Some(
                "branch names no node; no node carries this PR; body carries no Backlog-Closure \
                 trailer"
                    .to_string(),
            )
        } else {
            None
        }
    }
}

/// Compute the three binding keys of one PR against graph `entries`. The one
/// predicate behind both readers: the board's `pr_node_binding_untracked`
/// warning and the merge owner's refusal.
pub(crate) fn pr_binding_keys(
    number: i64,
    head_ref: &str,
    url: Option<&str>,
    body: Option<&str>,
    entries: &[Value],
) -> PrBinding {
    let real_ids: HashSet<&str> = entries.iter().filter_map(|e| s_str(e, "id")).collect();
    let branch: Vec<String> = branch_node_ids(head_ref)
        .into_iter()
        .filter(|nid| real_ids.contains(nid.as_str()))
        .collect();
    let trailer: Vec<String> = body
        .map(trailer_node_ids)
        .unwrap_or_default()
        .into_iter()
        .filter(|nid| real_ids.contains(nid.as_str()))
        .collect();
    // The graph's own back-pointer. An empty comparable URL answers nothing.
    let key = url.map(normalized_pr_url).unwrap_or_default();
    let mut backrefs: Vec<String> = Vec::new();
    if !key.is_empty() {
        for entry in entries {
            let Some(nid) = s_str(entry, "id") else {
                continue;
            };
            let hits = node_pr_refs(entry).iter().any(|(n, u)| {
                *n == number
                    && u.as_deref()
                        .map(normalized_pr_url)
                        .is_some_and(|u| u == key)
            });
            if hits {
                backrefs.push(nid.to_string());
            }
        }
    }
    backrefs.sort_unstable();
    PrBinding {
        branch,
        backrefs,
        trailer,
    }
}

/// Binding classification over already-fetched open-PR rows and graph
/// entries: the pure half of `read_prs`, returns (bound node rows, warnings).
/// Three keys, in order: delimiter-bounded branch matching; then - only when
/// the branch names nothing - the graph's own `(pr_number, pr_url)`
/// back-pointer, scoped by normalized URL because a pr_number is only unique
/// within one repository; then the body's exact `Backlog-Closure:` trailer
/// (mirrors _reconcile.classify_open_pr_bindings). The keys come from
/// [`pr_binding_keys`], the same predicate the merge owner reads.
fn classify_pr_bindings(rows: &[Value], entries: &[Value]) -> (Vec<Value>, Vec<String>) {
    let node_by_id: HashMap<&str, &Value> = entries
        .iter()
        .filter_map(|e| s_str(e, "id").map(|i| (i, e)))
        .collect();
    let mut warnings: Vec<String> = Vec::new();
    // First pass: which nodes have exactly one open PR. A trailer-resolved
    // row competes for its node like a branch-resolved one, so one node named
    // by a branch PR and a trailer PR reads ambiguous on both.
    let mut open_prs_by_node: HashMap<String, Vec<i64>> = HashMap::new();
    let mut parsed: Vec<(i64, Option<String>, String, PrBinding)> = Vec::new();
    for row in rows {
        let Some(number) = s_i64(row, "number") else {
            continue;
        };
        let head = s_str(row, "headRefName").unwrap_or("");
        if head.is_empty() {
            continue;
        }
        let keys = pr_binding_keys(
            number,
            head,
            row.get("url").and_then(Value::as_str),
            row.get("body").and_then(Value::as_str),
            entries,
        );
        if keys.branch.len() == 1 {
            open_prs_by_node
                .entry(keys.branch[0].clone())
                .or_default()
                .push(number);
        } else if keys.branch.is_empty() && keys.backrefs.is_empty() && keys.trailer.len() == 1 {
            // Sibling guard: branch and back-pointer both miss and the
            // trailer names exactly one real node, so the row competes for
            // that node like a branch-resolved one.
            open_prs_by_node
                .entry(keys.trailer[0].clone())
                .or_default()
                .push(number);
        }
        parsed.push((
            number,
            row.get("url").and_then(Value::as_str).map(str::to_string),
            head.to_string(),
            keys,
        ));
    }
    let mut bound: Vec<Value> = Vec::new();
    for (number, url, head, keys) in parsed {
        let unbound = keys.unbound_detail();
        let mut matched = keys.branch;
        if matched.is_empty() {
            match keys.backrefs.as_slice() {
                [] => {
                    if keys.trailer.is_empty() {
                        warnings.push(format!(
                            "pr_node_binding_untracked: #{number} {head} ({})",
                            unbound.unwrap_or_default()
                        ));
                        continue;
                    }
                    if keys.trailer.len() > 1 {
                        warnings.push(format!(
                            "pr_node_binding_ambiguous: #{number} -> {}",
                            keys.trailer.join(" ")
                        ));
                        continue;
                    }
                    matched = keys.trailer;
                }
                [only] => matched = vec![(*only).to_string()],
                many => {
                    warnings.push(format!(
                        "pr_node_binding_ambiguous: #{number} -> {}",
                        many.join(" ")
                    ));
                    continue; // ambiguous: a list-order guess is the wrong-node bind
                }
            }
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
    (bound, warnings)
}

/// Ids of every entry whose PR bindings contain `pr` (working graph plus
/// archive; duplicates possible, harmless: the same claim is re-read).
pub(crate) fn nodes_binding_pr<'a>(entries: &'a [Value], pr: i64) -> Vec<&'a str> {
    entries
        .iter()
        .filter(|e| node_pr_refs(e).iter().any(|(n, _)| *n == pr))
        .filter_map(|e| entry_id(e))
        .collect()
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
    if is_terminal(entry) && entry.get("completed_at").is_some_and(|v| !v.is_null()) {
        return "done".to_string();
    }
    s_str(entry, "status").unwrap_or("unknown").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spent_budget_reads_the_gate_source_unreadable() {
        let (read, warnings) = read_pr_gates(Path::new("."), &[1709], None);
        assert!(!read.is_ok());
        assert!(warnings.is_empty());
    }

    #[test]
    fn no_candidates_reads_the_gate_ok_and_empty() {
        let (read, warnings) = read_pr_gates(Path::new("."), &[], Some(Duration::from_secs(1)));
        assert!(read.is_ok());
        assert!(read.rows().is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn a_budget_killed_pr_listing_keeps_the_over_budget_verdict_in_both_queues() {
        let listing = SourceRead::over_budget(
            "gh pr list --state open: killed at its 0.8s slice of the board budget; the source did not fail",
        );
        let (mergeable, undriven) = split_failed_listing(&listing);
        assert!(mergeable.over_budget);
        assert!(undriven.over_budget);
        assert!(undriven
            .error
            .as_deref()
            .unwrap_or("")
            .starts_with("undriven_pr:"));
    }

    #[test]
    fn branch_ids_never_match_a_partial_hex_prefix() {
        assert_eq!(
            branch_node_ids("feature/x-aaaa-1234"),
            vec!["x-aaaa".to_string()]
        );
        assert_eq!(
            branch_node_ids("x-5b667-fixes-x-bbbb"),
            vec!["x-5b667".to_string(), "x-bbbb".to_string()]
        );
        // Uppercase is not id body ([0-9a-f], not [0-9a-fA-F]): the hex run
        // stops at 'E', so "x-cccc" binds and the tail never reads as id.
        assert_eq!(branch_node_ids("x-cccc-EF12"), vec!["x-cccc".to_string()]);
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

    fn pr_row(number: i64, head: &str, url: &str) -> Value {
        json!({"number": number, "headRefName": head, "url": url})
    }

    fn pr_row_with_body(number: i64, head: &str, url: &str, body: &str) -> Value {
        let mut row = pr_row(number, head, url);
        row["body"] = json!(body);
        row
    }

    #[test]
    fn binding_warns_untracked_when_no_key_names_the_node() {
        // AC5: an unbindable PR is named in the warnings instead of dropped
        // from every queue.
        let entries = vec![json!({"id": "x-dddd"})];
        let rows = vec![pr_row(
            5,
            "chore/tidy-docs",
            "https://github.com/o/r/pull/5",
        )];
        let (bound, warnings) = classify_pr_bindings(&rows, &entries);
        assert!(bound.is_empty());
        let warning = warnings
            .iter()
            .find(|w| w.contains("pr_node_binding_untracked"))
            .expect("untracked warning");
        assert!(warning.contains("#5") && warning.contains("chore/tidy-docs"));
    }

    #[test]
    fn binding_reads_the_body_trailer_and_warns_missing_until_the_ref_lands() {
        // AC1: a trailer-only row resolves through the body; without a
        // back-pointer it takes the existing missing warning, with one it
        // binds.
        let entries = vec![json!({"id": "x-eeee"})];
        let rows = vec![pr_row_with_body(
            5,
            "fix/descriptive-name",
            "https://github.com/o/r/pull/5",
            "Summary.\n\nBacklog-Closure: x-eeee\n",
        )];
        let (bound, warnings) = classify_pr_bindings(&rows, &entries);
        assert!(bound.is_empty());
        assert!(warnings
            .iter()
            .any(|w| w.contains("pr_node_binding_missing") && w.contains("x-eeee")));

        let entries = vec![json!({
            "id": "x-eeee", "pr_number": 5,
            "pr_url": "https://github.com/o/r/pull/5",
        })];
        let (bound, warnings) = classify_pr_bindings(&rows, &entries);
        assert_eq!(bound.len(), 1);
        assert_eq!(s_str(&bound[0], "id"), Some("x-eeee"));
        assert!(warnings.is_empty());
    }

    #[test]
    fn binding_ignores_prose_and_reads_only_the_last_trailer_line() {
        // Only a line starting with the trailer key counts (prose never
        // becomes a claim), and only the LAST such line wins.
        let entries = vec![json!({"id": "x-eeee"}), json!({"id": "x-prose"})];
        let rows = vec![pr_row_with_body(
            5,
            "chore/tidy-docs",
            "https://github.com/o/r/pull/5",
            "this also fixes x-prose; the Backlog-Closure trailer is documented elsewhere.\n",
        )];
        let (bound, warnings) = classify_pr_bindings(&rows, &entries);
        assert!(bound.is_empty());
        assert!(warnings
            .iter()
            .any(|w| w.contains("pr_node_binding_untracked")));

        let rows = vec![pr_row_with_body(
            5,
            "chore/tidy-docs",
            "https://github.com/o/r/pull/5",
            "backlog-closure: x-prose\nBacklog-Closure: x-eeee",
        )];
        let entries = vec![json!({"id": "x-eeee"})];
        let (bound, warnings) = classify_pr_bindings(&rows, &entries);
        assert!(bound.is_empty());
        assert!(warnings
            .iter()
            .any(|w| w.contains("pr_node_binding_missing") && w.contains("x-eeee")));
        assert!(!warnings.iter().any(|w| w.contains("x-prose")));

        let entries = vec![json!({
            "id": "x-eeee", "pr_number": 5,
            "pr_url": "https://github.com/o/r/pull/5",
        })];
        let (bound, warnings) = classify_pr_bindings(&rows, &entries);
        assert_eq!(bound.len(), 1);
        assert_eq!(s_str(&bound[0], "id"), Some("x-eeee"));
        assert!(warnings.is_empty());
    }

    #[test]
    fn binding_refuses_when_the_trailer_names_several_real_nodes() {
        // Several trailer claims are ambiguous, never a list-order pick.
        let url = "https://github.com/o/r/pull/5";
        let entries = vec![json!({"id": "x-dddd"}), json!({"id": "x-aaaa"})];
        let rows = vec![pr_row_with_body(
            5,
            "chore/no-node-here",
            url,
            "Backlog-Closure: x-dddd, x-aaaa",
        )];
        let (bound, warnings) = classify_pr_bindings(&rows, &entries);
        assert!(bound.is_empty());
        assert!(warnings
            .iter()
            .any(|w| w.contains("pr_node_binding_ambiguous")
                && w.contains("x-dddd")
                && w.contains("x-aaaa")));
    }

    #[test]
    fn binding_sibling_guard_makes_branch_and_trailer_prs_ambiguous() {
        // One node named by a branch PR and a trailer PR has two open PRs,
        // so neither row binds alone.
        let entries = vec![json!({"id": "x-dddd"})];
        let rows = vec![
            pr_row(5, "feature/x-dddd", "https://github.com/o/r/pull/5"),
            pr_row_with_body(
                6,
                "target/other-work",
                "https://github.com/o/r/pull/6",
                "Backlog-Closure: x-dddd",
            ),
        ];
        let (bound, warnings) = classify_pr_bindings(&rows, &entries);
        assert!(bound.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn binding_binds_through_the_graphs_reverse_key_when_the_branch_names_no_node() {
        // AC1/AC6: the graph's own back-pointer binds a node-less branch.
        // Modeled on a real open PR whose branch carried no id while its node
        // carried the back-pointer the resolver never read.
        let entries = vec![json!({
            "id": "x-ffff", "pr_number": 1476,
            "pr_url": "https://github.com/o/r/pull/1476",
        })];
        let rows = vec![pr_row(
            1476,
            "fix/review-cap-invocation-gate",
            "https://github.com/o/r/pull/1476",
        )];
        let (bound, warnings) = classify_pr_bindings(&rows, &entries);
        assert_eq!(bound.len(), 1);
        assert_eq!(s_str(&bound[0], "id"), Some("x-ffff"));
        assert_eq!(s_i64(&bound[0], "pr_number"), Some(1476));
        assert!(warnings.is_empty());
    }

    #[test]
    fn binding_refuses_when_two_nodes_carry_the_pr() {
        // AC2/AC6: several reverse hits are ambiguous, never a list-order pick.
        let url = "https://github.com/o/r/pull/1476";
        let entries = vec![
            json!({"id": "x-dddd", "pr_number": 1476, "pr_url": url}),
            json!({"id": "x-aaaa", "pr_number": 1476, "pr_url": url}),
        ];
        let rows = vec![pr_row(1476, "chore/no-node-here", url)];
        let (bound, warnings) = classify_pr_bindings(&rows, &entries);
        assert!(bound.is_empty());
        assert!(warnings
            .iter()
            .any(|w| w.contains("pr_node_binding_ambiguous")
                && w.contains("x-dddd")
                && w.contains("x-aaaa")));
    }

    #[test]
    fn binding_reverse_key_is_scoped_by_url() {
        // AC4/AC6: a same-numbered PR on another owner/repo never binds.
        let entries = vec![json!({
            "id": "x-dddd", "pr_number": 1476,
            "pr_url": "https://github.com/o/other/pull/1476",
        })];
        let rows = vec![pr_row(
            1476,
            "chore/tidy-docs",
            "https://github.com/o/r/pull/1476",
        )];
        let (bound, warnings) = classify_pr_bindings(&rows, &entries);
        assert!(bound.is_empty());
        // The verdict is unchanged (never binds cross-repo); since the
        // row is named in the warnings instead of dropped silently.
        assert!(warnings
            .iter()
            .any(|w| w.contains("pr_node_binding_untracked") && w.contains("#1476")));
    }

    #[test]
    fn the_shared_predicate_reports_bound_through_each_of_the_three_keys() {
        let entries = vec![json!({"id": "x-dddd"})];
        let url = "https://github.com/o/r/pull/5";
        // Branch key: the head names a real node.
        let keys = pr_binding_keys(5, "feature/x-dddd", Some(url), None, &entries);
        assert_eq!(keys.branch, vec!["x-dddd".to_string()]);
        assert_eq!(keys.unbound_detail(), None);
        // Trailer key: a nodeless head, the node named on the body.
        let keys = pr_binding_keys(
            5,
            "fix/descriptive",
            Some(url),
            Some("Backlog-Closure: x-dddd"),
            &entries,
        );
        assert_eq!(keys.trailer, vec!["x-dddd".to_string()]);
        assert_eq!(keys.unbound_detail(), None);
        // Backref key: a nodeless head, the node carries the back-pointer.
        let carrying = vec![json!({
            "id": "x-dddd", "pr_number": 5, "pr_url": url,
        })];
        let keys = pr_binding_keys(5, "fix/descriptive", Some(url), None, &carrying);
        assert_eq!(keys.backrefs, vec!["x-dddd".to_string()]);
        assert_eq!(keys.unbound_detail(), None);
    }

    #[test]
    fn the_shared_predicate_reports_unbound_when_all_three_keys_miss() {
        // AC2-ERR: no node id on the branch, no carrying node, and a body
        // that is absent or names only an id the graph does not have.
        let entries = vec![json!({"id": "x-dddd"})];
        let url = "https://github.com/o/r/pull/5";
        for body in [None, Some("Backlog-Closure: x-9999")] {
            let keys = pr_binding_keys(5, "docs/crown-succeed-faq", Some(url), body, &entries);
            assert_eq!(
                keys.unbound_detail().as_deref(),
                Some(
                    "branch names no node; no node carries this PR; body carries no \
                     Backlog-Closure trailer"
                ),
                "body {body:?}"
            );
        }
    }
}
