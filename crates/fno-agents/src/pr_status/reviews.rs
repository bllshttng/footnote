//! The review-side seams the composer composes: optional reviews (GraphQL
//! through the sanctioned `fno do pr graphql-exec` lane), the review-coverage
//! row (journal reads plus at one recompute through the producer verb), the
//! review-lane predicate, and review activity (the claims hold plus the
//! worktree probe). Ported from `fno.pr._reviews` and `fno.pr._review_hold`;
//! both Python modules stay for the merge and review verbs that still import
//! them.

use crate::loopcheck::{resolve_review_inputs, ReviewInputs};
use serde_json::{json, Value};
use std::path::Path;

/// One GraphQL page of review threads (byte-identical to `_reviews.py`).
const THREADS_QUERY: &str = "query($owner:String!,$name:String!,$number:Int!,$cursor:String){\
repository(owner:$owner,name:$name){pullRequest(number:$number){\
reviewThreads(first:100,after:$cursor){\
pageInfo{hasNextPage endCursor}\
nodes{isResolved isOutdated comments(first:1){nodes{author{login}}}}\
}}}}";

/// The read-failure sentinel: distinct from an empty list, so "read failed"
/// never reads as "nothing posted" (US4).
fn reviews_unknown() -> Value {
    json!({
        "optional_reviews": "unknown",
        "optional_reviews_unresolved": Value::Null,
        "optional_reviews_resolved_unchanged": Value::Null,
    })
}

/// One `fno do pr graphql-exec` run; stdout on exit 0, else None. The lane is
/// the quota broker's own door, so reserve accounting stays the one law.
fn graphql_exec(cwd: &Path, args: &[String]) -> Option<String> {
    let mut argv = vec![
        "do".to_string(),
        "pr".to_string(),
        "graphql-exec".to_string(),
        "--purpose".to_string(),
        "discretionary".to_string(),
        "--".to_string(),
    ];
    argv.extend_from_slice(args);
    let out = std::process::Command::new("fno")
        .args(&argv)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    if text.trim().is_empty() {
        return None;
    }
    Some(text)
}

/// Drop a trailing `[bot]` for display; GitHub appends it to app logins.
fn strip_bot(login: &str) -> String {
    if login.len() >= 5 && login[login.len() - 5..].eq_ignore_ascii_case("[bot]") {
        login[..login.len() - 5].to_string()
    } else {
        login.to_string()
    }
}

/// Case-insensitive substring match after dropping a `[bot]` suffix: the
/// `_reviewer_matches` rule.
fn reviewer_matches(login: &str, names: &[String]) -> bool {
    if login.is_empty() {
        return false;
    }
    let mut stripped = login.to_lowercase();
    if stripped.ends_with("[bot]") {
        stripped.truncate(stripped.len() - 5);
    }
    names
        .iter()
        .any(|n| !n.is_empty() && stripped.contains(&n.to_lowercase()))
}

/// `owner/repo` from a `pr view --json url` value.
fn slug_from_url(url: &str) -> Option<String> {
    let rest = url.split("github.com/").nth(1)?;
    let mut parts = rest.split('/');
    let owner = parts.next()?.trim();
    let name = parts.next()?.trim();
    if owner.is_empty() || name.is_empty() {
        return None;
    }
    Some(format!("{owner}/{}", name.trim_end_matches(".git")))
}

/// Optional-review presence and thread counts for one PR. Any read failure
/// answers the unknown sentinel and never touches the CI verdict.
pub(crate) fn optional_reviews(cwd: &Path, pr: u64) -> Value {
    let inputs = resolve_review_inputs(cwd, None, None, None, None, None);
    let names = inputs.optional_reviewer_names.clone();
    let Some(raw) = graphql_exec(
        cwd,
        &[
            "pr".into(),
            "view".into(),
            pr.to_string(),
            "--json".into(),
            "reviews,url".into(),
        ],
    ) else {
        return reviews_unknown();
    };
    let Ok(data) = serde_json::from_str::<Value>(&raw) else {
        return reviews_unknown();
    };
    if !data.is_object() {
        return reviews_unknown();
    }
    let url = data.get("url").and_then(Value::as_str).unwrap_or("");
    let Some(slug) = slug_from_url(url) else {
        return reviews_unknown();
    };
    let Some(threads) = fetch_threads(cwd, pr, &slug) else {
        return reviews_unknown();
    };
    compose_reviews(&data, &names, &threads)
}

/// Review-level presence plus thread counts, composed from the two reads.
fn compose_reviews(data: &Value, names: &[String], threads: &[(String, bool, bool)]) -> Value {
    let mut by_author: std::collections::BTreeMap<String, Value> =
        std::collections::BTreeMap::new();
    for review in data
        .get("reviews")
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or_default()
    {
        let login = review
            .pointer("/author/login")
            .and_then(Value::as_str)
            .unwrap_or("");
        if !reviewer_matches(login, names) {
            continue;
        }
        let key = strip_bot(login).to_lowercase();
        let row = by_author.entry(key).or_insert_with(
            || json!({"author": strip_bot(login), "state": Value::Null, "inline_count": 0}),
        );
        if let Some(state) = review.get("state").and_then(Value::as_str) {
            if !state.is_empty() {
                row["state"] = json!(state);
            }
        }
    }
    let mut unresolved = 0i64;
    let mut resolved_unchanged = 0i64;
    for (author, resolved, outdated) in threads {
        if !reviewer_matches(author, names) {
            continue;
        }
        let key = strip_bot(author).to_lowercase();
        let row = by_author.entry(key).or_insert_with(
            || json!({"author": strip_bot(author), "state": Value::Null, "inline_count": 0}),
        );
        row["inline_count"] = json!(row["inline_count"].as_i64().unwrap_or(0) + 1);
        if !*resolved {
            unresolved += 1;
        } else if !*outdated {
            resolved_unchanged += 1;
        }
    }
    let mut list: Vec<Value> = by_author.into_values().collect();
    list.sort_by(|a, b| {
        a.get("author")
            .and_then(Value::as_str)
            .unwrap_or("")
            .cmp(b.get("author").and_then(Value::as_str).unwrap_or(""))
    });
    json!({
        "optional_reviews": list,
        "optional_reviews_unresolved": unresolved,
        "optional_reviews_resolved_unchanged": resolved_unchanged,
    })
}

type Thread = (String, bool, bool);

/// The PR's review threads, paged; None on any failure. `gh api graphql`
/// exits 0 on a GraphQL-level error too, so the body is checked as well.
fn fetch_threads(cwd: &Path, pr: u64, slug: &str) -> Option<Vec<Thread>> {
    let (owner, name) = slug.split_once('/')?;
    if owner.is_empty() || name.is_empty() {
        return None;
    }
    let mut threads: Vec<Thread> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..50 {
        let mut args = vec![
            "api".to_string(),
            "graphql".to_string(),
            "-f".to_string(),
            format!("query={THREADS_QUERY}"),
            "-f".to_string(),
            format!("owner={owner}"),
            "-f".to_string(),
            format!("name={name}"),
            "-F".to_string(),
            format!("number={pr}"),
        ];
        if let Some(c) = &cursor {
            args.push("-f".into());
            args.push(format!("cursor={c}"));
        }
        let raw = graphql_exec(cwd, &args)?;
        let data: Value = serde_json::from_str(&raw).ok()?;
        if !data.is_object() {
            return None;
        }
        let pr_node = data
            .pointer("/data/repository/pullRequest")
            .cloned()
            .unwrap_or(Value::Null);
        if data.get("errors").is_some_and(|e| !e.is_null()) || pr_node.is_null() {
            return None;
        }
        for node in pr_node
            .pointer("/reviewThreads/nodes")
            .and_then(Value::as_array)
            .map(|a| a.as_slice())
            .unwrap_or_default()
        {
            let resolved = node.get("isResolved").and_then(Value::as_bool);
            let outdated = node.get("isOutdated").and_then(Value::as_bool);
            let (Some(resolved), Some(outdated)) = (resolved, outdated) else {
                return None;
            };
            let author = node
                .pointer("/comments/nodes/0/author/login")
                .and_then(Value::as_str)
                .unwrap_or("");
            threads.push((author.to_string(), resolved, outdated));
        }
        let more = pr_node
            .pointer("/reviewThreads/pageInfo/hasNextPage")
            .and_then(Value::as_bool);
        let end = pr_node
            .pointer("/reviewThreads/pageInfo/endCursor")
            .and_then(Value::as_str);
        match (more, end) {
            (Some(true), Some(c)) if !c.is_empty() => cursor = Some(c.to_string()),
            _ => break,
        }
    }
    Some(threads)
}

// ---------------------------------------------------------------------------
// The review-coverage row
// ---------------------------------------------------------------------------

/// Freshness labels that count toward coverage; `carried_interdiff(n=...)` is
/// membership by prefix.
fn counted_freshness(label: &Value) -> bool {
    let Some(s) = label.as_str() else {
        return false;
    };
    matches!(
        s,
        "fresh" | "carried_base_sync" | "carried_docs_only" | "carried_subset"
    ) || (s.starts_with("carried_interdiff(") && s.ends_with(')'))
}

fn verdict_row_ok(v: &Value) -> bool {
    v.is_object()
        && matches!(
            v.get("verdict").and_then(Value::as_str),
            Some("reviewed" | "stale" | "refused" | "errored" | "absent")
        )
        && matches!(
            v.get("producer").and_then(Value::as_str),
            Some("github_app" | "local_attestation")
        )
        && v.get("name")
            .and_then(Value::as_str)
            .is_some_and(|n| !n.is_empty())
}

fn verdicts_as_stored(data: &Value) -> Vec<Value> {
    let Some(verdicts) = data.get("verdicts").and_then(Value::as_array) else {
        return Vec::new();
    };
    verdicts
        .iter()
        .filter(|v| v.is_object())
        .map(|v| {
            let mut row = v.clone();
            if !matches!(
                row.get("verdict").and_then(Value::as_str),
                Some("reviewed" | "stale")
            ) {
                if let Some(obj) = row.as_object_mut() {
                    obj.remove("freshness");
                }
            }
            row
        })
        .collect()
}

fn stale_verdicts(verdicts: &[Value]) -> Vec<Value> {
    verdicts
        .iter()
        .filter(|v| {
            matches!(
                v.get("verdict").and_then(Value::as_str),
                Some("reviewed" | "stale")
            ) && v.get("freshness").and_then(Value::as_str) == Some("stale")
        })
        .map(|v| {
            json!({
                "name": v.get("name").cloned().unwrap_or(Value::Null),
                "producer": v.get("producer").cloned().unwrap_or(Value::Null),
                "reviewed_sha": v.get("reviewed_sha").cloned().unwrap_or(Value::Null),
                "freshness": v.get("freshness").cloned().unwrap_or(Value::Null),
            })
        })
        .collect()
}

fn tiling_chain(data: &Value) -> Vec<String> {
    match data.get("range_tiling") {
        Some(t) if t.get("tiled").and_then(Value::as_bool) == Some(true) => t
            .get("chain_heads")
            .and_then(Value::as_array)
            .map(|heads| {
                heads
                    .iter()
                    .filter_map(|h| h.as_str())
                    .filter(|h| !h.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// One known outcome from validated per-reviewer verdicts; None fails closed.
fn derive_review_state(
    coverage: &str,
    verdicts: &[Value],
    chain: &[String],
    approval_satisfies: bool,
    rounds_exhausted: bool,
) -> Option<&'static str> {
    if coverage == "unknown" {
        return None;
    }
    if verdicts.is_empty() || verdicts.iter().any(|v| !verdict_row_ok(v)) {
        return None;
    }
    if rounds_exhausted && coverage == "covered" {
        return Some("reviewed");
    }
    // A human GitHub approval counts only when the flag is on AND the
    // approver is provably not the PR author; `author_approval` absent reads
    // as the exclude side, the fail-closed direction.
    let human_counts = |v: &Value| {
        v.get("human_approval").and_then(Value::as_bool) != Some(true)
            || (approval_satisfies
                && v.get("author_approval").and_then(Value::as_bool) == Some(false))
    };
    if verdicts.iter().any(|v| {
        v.get("verdict").and_then(Value::as_str) == Some("reviewed")
            && human_counts(v)
            && (counted_freshness(v.get("freshness").unwrap_or(&Value::Null))
                || v.get("reviewed_sha")
                    .and_then(Value::as_str)
                    .is_some_and(|sha| chain.iter().any(|c| c.as_str() == sha)))
    }) {
        return Some("reviewed");
    }
    if verdicts.iter().any(|v| {
        v.get("verdict").and_then(Value::as_str) == Some("refused")
            && v.get("required").and_then(Value::as_bool) != Some(false)
    }) {
        return Some("reviewer_refused");
    }
    Some("unreviewed")
}

/// Shape one event and invalidate any unproven covered verdict.
fn shape_review_coverage(data: &Value, head: Option<&str>, approval_satisfies: bool) -> Value {
    let mut shaped = data.clone();
    let verdicts = verdicts_as_stored(data);
    let stale = stale_verdicts(&verdicts);
    let chain = tiling_chain(data);
    let spent = data.get("rounds_exhausted").and_then(Value::as_bool) == Some(true);
    let coverage_word = data.get("coverage").and_then(Value::as_str).unwrap_or("");
    let state = derive_review_state(coverage_word, &verdicts, &chain, approval_satisfies, spent);
    let obj = shaped.as_object_mut().expect("row is an object");
    obj.insert("verdicts".into(), Value::Array(verdicts.clone()));
    obj.insert("stale_verdicts".into(), Value::Array(stale));
    match state {
        Some(s) => {
            obj.insert("review_state".into(), json!(s));
        }
        None => {
            obj.remove("review_state");
        }
    }
    if coverage_word != "covered" || spent {
        return shaped;
    }
    let raw = data.get("verdicts").and_then(Value::as_array);
    let malformed =
        raw.is_none_or(|r| r.is_empty() || r.iter().any(|v| !v.is_object() || !verdict_row_ok(v)));
    let reviewed: Vec<&Value> = verdicts
        .iter()
        .filter(|v| v.get("verdict").and_then(Value::as_str) == Some("reviewed"))
        .collect();
    let valid = reviewed
        .iter()
        .filter(|v| {
            counted_freshness(v.get("freshness").unwrap_or(&Value::Null))
                || v.get("reviewed_sha")
                    .and_then(Value::as_str)
                    .is_some_and(|sha| chain.iter().any(|c| c.as_str() == sha))
        })
        .count();
    if malformed || reviewed.is_empty() || valid != reviewed.len() {
        shaped["coverage"] = json!("uncovered");
    }
    let _ = head;
    shaped
}

/// Latest `review_coverage` data for a PR across both journals: the project
/// log unscoped, the global log scoped by the repo slug. Newest ts wins; a
/// same-ts tie takes the SAFER word (the uncovered one).
fn latest_coverage_row(cwd: &Path, inputs: &ReviewInputs, pr: u64) -> (Option<Value>, String) {
    let scan = |journal: &Path, repo_scope: Option<&str>| -> (Option<Value>, String) {
        let text = crate::event_store::journal_text(journal, &["review_coverage"]);
        let mut latest: Option<Value> = None;
        let mut latest_ts = String::new();
        for line in text.lines() {
            if !line.contains("review_coverage") {
                continue;
            }
            let Ok(ev) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if ev.get("type").and_then(Value::as_str) != Some("review_coverage") {
                continue;
            }
            let data = ev.get("data").cloned().unwrap_or(Value::Null);
            if !data.is_object() {
                continue;
            }
            if data.get("pr").and_then(Value::as_i64) != Some(pr as i64) {
                continue;
            }
            if let Some(slug) = repo_scope {
                if data.get("repo").and_then(Value::as_str) != Some(slug) {
                    continue;
                }
            }
            latest = Some(data);
            latest_ts = ev
                .get("ts")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
        }
        (latest, latest_ts)
    };
    let (mut best, mut best_ts) = scan(&inputs.project_events, None);
    let (other, other_ts) = scan(&inputs.global_events, Some(inputs.repo_slug.as_str()));
    let covered = |v: &Option<Value>| {
        v.as_ref()
            .and_then(|d| d.get("coverage"))
            .and_then(Value::as_str)
            == Some("covered")
    };
    if other.is_some()
        && (best.is_none()
            || other_ts > best_ts
            || (other_ts == best_ts && !covered(&other) && covered(&best)))
    {
        best = other;
        best_ts = other_ts;
    }
    (best, best_ts)
}

/// Whether a head-matching stored NO row was overtaken by a later in-scope
/// attestation at the same head. Any verdict overtakes: a later fail moved
/// the round and the count the row reports.
fn uncovered_row_overtaken(data: &Value, row_ts: &str, inputs: &ReviewInputs, head: &str) -> bool {
    if head.is_empty() || row_ts.is_empty() {
        return false;
    }
    let word = data.get("coverage").and_then(Value::as_str).unwrap_or("");
    let posture_satisfied = data
        .pointer("/review_posture/posture_satisfied")
        .and_then(Value::as_bool);
    let stored_no = word == "uncovered" || (word == "covered" && posture_satisfied == Some(false));
    if !stored_no || data.get("head_sha").and_then(Value::as_str) != Some(head) {
        return false;
    }
    let row_at = crate::event_store::parse_rfc3339_ms(row_ts);
    let later = |journal: &Path, repo_scope: Option<&str>| -> bool {
        let text = crate::event_store::journal_text(journal, &["review_attestation"]);
        for line in text.lines() {
            let Ok(ev) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let data = ev.get("data").cloned().unwrap_or(Value::Null);
            if data.get("head_sha").and_then(Value::as_str) != Some(head) {
                continue;
            }
            if let Some(slug) = repo_scope {
                if data.get("repo").and_then(Value::as_str) != Some(slug) {
                    continue;
                }
            }
            let ts = ev
                .get("ts")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let after = match (row_at, crate::event_store::parse_rfc3339_ms(&ts)) {
                (Some(base), Some(other)) => other > base,
                _ => ts.as_str() > row_ts,
            };
            if after {
                return true;
            }
        }
        false
    };
    later(&inputs.project_events, None)
        || later(&inputs.global_events, Some(inputs.repo_slug.as_str()))
}

const PIN_PREFIX: &str = "coverage row pinned to ";

fn pinned_note(row: &Value, ts: &str) -> String {
    let word = row.get("coverage").and_then(Value::as_str).unwrap_or("");
    if !matches!(word, "uncovered" | "unknown") {
        return String::new();
    }
    let Some(sha) = row
        .get("head_sha")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return String::new();
    };
    let when = if ts.is_empty() {
        String::new()
    } else {
        format!(" at {ts}")
    };
    format!("{PIN_PREFIX}{}{}", &sha[..sha.len().min(9)], when)
}

fn split_pin_note(note: &str) -> (String, String) {
    let parts: Vec<&str> = note
        .split(';')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    let pins: Vec<&str> = parts
        .iter()
        .copied()
        .filter(|p| p.starts_with(PIN_PREFIX))
        .collect();
    let rest: Vec<&str> = parts
        .iter()
        .copied()
        .filter(|p| !p.starts_with(PIN_PREFIX))
        .collect();
    (rest.join("; "), pins.join("; "))
}

/// The `review_coverage` verdict for a PR: the latest event row, shaped
/// against the head, recomputed at most ONCE through the producer verb when
/// `recompute` is set and the row is absent, head-mismatched, unknown, or
/// overtaken. Event-read failures degrade to the unknown sentinel.
pub(crate) fn read_review_coverage(
    cwd: &Path,
    pr: u64,
    head: Option<&str>,
    recompute: bool,
    recompute_postureless: bool,
) -> Value {
    let inputs = resolve_review_inputs(cwd, None, None, None, None, None);
    let approval_satisfies = inputs.approval_satisfies;
    let (raw, raw_ts) = latest_coverage_row(cwd, &inputs, pr);
    let mut note = String::new();
    let mut data = raw
        .as_ref()
        .map(|d| shape_review_coverage(d, head, approval_satisfies));
    if recompute {
        let ev_head = raw
            .as_ref()
            .and_then(|d| d.get("head_sha"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let mismatch =
            head.is_some() && raw.is_some() && !ev_head.is_empty() && head != Some(ev_head);
        let unusable = data.as_ref().is_some_and(|d| {
            d.get("coverage").and_then(Value::as_str) == Some("unknown")
                || (d.get("coverage").and_then(Value::as_str) == Some("covered")
                    && !d
                        .as_object()
                        .is_some_and(|o| o.contains_key("review_posture"))
                    && recompute_postureless)
        });
        let overtaken = raw
            .as_ref()
            .map(|d| uncovered_row_overtaken(d, &raw_ts, &inputs, head.unwrap_or("")))
            .unwrap_or(false);
        if raw.is_none() || mismatch || unusable || overtaken {
            let mut args = vec![
                "--cwd".to_string(),
                cwd.display().to_string(),
                "--pr".to_string(),
                pr.to_string(),
            ];
            if let Some(h) = head.filter(|h| !h.is_empty()) {
                args.push("--head".into());
                args.push(h.to_string());
            }
            let (code, stdout) = crate::loopcheck::run_review_coverage_capture(&args);
            if matches!(code, 0 | 3 | 4) {
                let (fresh, fresh_ts) = latest_coverage_row(cwd, &inputs, pr);
                if let Some(f) = fresh.as_ref() {
                    data = Some(shape_review_coverage(f, head, approval_satisfies));
                    note = "recomputed".to_string();
                    if code == 4 {
                        let why = serde_json::from_str::<Value>(&stdout)
                            .ok()
                            .and_then(|p| {
                                p.get("reason").and_then(Value::as_str).map(str::to_string)
                            })
                            .unwrap_or_else(|| "gh read failed (exit 4)".into());
                        note = format!("recompute degraded to unknown: {why}");
                    }
                    let pin = data
                        .as_ref()
                        .map(|d| pinned_note(d, &fresh_ts))
                        .unwrap_or_default();
                    return finish(data, &note, &pin);
                }
                note = "recompute produced no row".to_string();
                return finish(None, &note, "");
            }
            let why = stdout
                .trim()
                .lines()
                .last()
                .unwrap_or("recompute failed")
                .to_string();
            note = format!("recompute unavailable: {why}");
            if data.is_none() {
                data = Some(json!({
                    "coverage": "unknown",
                    "reviewed_count": 0,
                    "stale_verdicts": [],
                    "verdicts": [],
                    "reason": why,
                }));
            }
        }
    }
    let pin = data
        .as_ref()
        .map(|d| pinned_note(d, &raw_ts))
        .unwrap_or_default();
    finish(data, &note, &pin)
}

/// The shaped dict `run_status` reads: the producer's counts, the round
/// budget keys verbatim, and the recompute/pin notes split onto their own
/// keys. `latest` may be None: the read failed, and the answer is the
/// unknown sentinel with its own note, never a synthesized row.
fn finish(data: Option<Value>, note: &str, pin: &str) -> Value {
    let Some(latest) = data else {
        return json!({
            "coverage": "unknown",
            "reviewed_count": Value::Null,
            "self_attested_count": Value::Null,
            "head_sha": Value::Null,
            "stale_verdicts": [],
            "note": "coverage probe failed",
        });
    };
    let mut shaped = json!({
        "coverage": latest.get("coverage").cloned().unwrap_or(json!("unknown")),
        "reviewed_count": latest.get("reviewed_count").cloned().unwrap_or(Value::Null),
        "passed_count": latest.get("passed_count").cloned().unwrap_or(Value::Null),
        "self_attested_count": latest.get("self_attested_count").cloned().unwrap_or(Value::Null),
        "head_sha": latest.get("head_sha").cloned().unwrap_or(Value::Null),
        "stale_verdicts": latest.get("stale_verdicts").cloned().unwrap_or(json!([])),
    });
    let obj = shaped.as_object_mut().unwrap();
    for key in ["rounds_used", "rounds_max", "rounds_exhausted"] {
        if latest.get(key).is_some_and(|v| !v.is_null()) {
            obj.insert(key.into(), latest.get(key).cloned().unwrap());
        }
    }
    if let Some(reason) = latest
        .get("reason")
        .filter(|r| !r.is_null() && r.as_str() != Some(""))
    {
        obj.insert("reason".into(), reason.clone());
    }
    if let Some(state) = latest.get("review_state").filter(|s| {
        matches!(
            s.as_str(),
            Some("reviewed" | "unreviewed" | "reviewer_refused")
        )
    }) {
        obj.insert("review_state".into(), state.clone());
    }
    if let Some(verdicts) = latest.get("verdicts") {
        obj.insert("verdicts".into(), verdicts.clone());
    }
    let joined = format!("{note}; {pin}");
    let (recompute_note, pin_note) = split_pin_note(&joined);
    let recompute_note = recompute_note.trim();
    let pin_note = pin_note.trim();
    if !recompute_note.is_empty() {
        obj.insert("recompute".into(), json!(recompute_note));
    }
    if !pin_note.is_empty() {
        obj.insert("coverage_pin".into(), json!(pin_note));
    }
    shaped
}

// ---------------------------------------------------------------------------
// The review-lane predicate
// ---------------------------------------------------------------------------

/// Whether review is required for this PR: a configured lane, OR the
/// self-review floor for a code payload on a stock install. Fail-closed.
pub(crate) fn review_lane(cwd: &Path, pr: u64) -> bool {
    let inputs = resolve_review_inputs(cwd, None, None, None, None, None);
    if inputs.lane_configured || inputs.code_review_configured {
        return true;
    }
    let floor_applies = crate::loopcheck::self_review_floor_applies(
        inputs.author_harness.as_deref(),
        inputs.author_harness_pinned_none,
    );
    inputs.self_review_floor_on && floor_applies && payload_is_code(cwd, pr)
}

/// Whether the PR's diff carries a code payload: CODE iff any changed file
/// is not documentation. Fails closed: a failed read classifies as code, so
/// a degraded probe cannot bypass the floor. An empty surfaced diff is not
/// code: nothing to review, no gate.
fn payload_is_code(cwd: &Path, pr: u64) -> bool {
    let Some(url) = git_origin_url(cwd) else {
        return true;
    };
    let Some(slug) = crate::merge_gates::repo_slug_from_origin(&url) else {
        return true;
    };
    let mut names: Vec<String> = Vec::new();
    for page in 1..=10 {
        let out = std::process::Command::new("gh")
            .args([
                "api",
                &format!("repos/{slug}/pulls/{pr}/files?per_page=100&page={page}"),
            ])
            .current_dir(cwd)
            .output();
        let Ok(out) = out else {
            return true;
        };
        if !out.status.success() {
            return true;
        }
        let Ok(rows) = serde_json::from_str::<Value>(&String::from_utf8_lossy(&out.stdout)) else {
            return true;
        };
        let Some(list) = rows.as_array() else {
            return true;
        };
        names.extend(
            list.iter()
                .filter_map(|r| r.get("filename").and_then(Value::as_str))
                .map(str::to_string),
        );
        if list.len() < 100 {
            break;
        }
    }
    names.is_empty()
        || names
            .iter()
            .any(|p| !crate::loopcheck::is_documentation_path(p))
}

fn git_origin_url(cwd: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

// ---------------------------------------------------------------------------
// Review activity: the registered hold, then the worktree probe
// ---------------------------------------------------------------------------

const REVIEW_IN_FLIGHT: &str = "review_in_flight";
const REVIEW_HOLD_UNREADABLE: &str = "review_hold_unreadable";
const WORKTREE_DIRTY: &str = "worktree_dirty";
const WORKTREE_HEAD_MISMATCH: &str = "worktree_head_mismatch";
const WORKTREE_PROBE_FAILED: &str = "worktree_probe_failed";

fn not_reached(note: &str) -> Value {
    json!({"probed": false, "path": null, "dirty": null, "head": null, "note": note})
}

/// Is a review of `branch` in flight? The TTL claim first, then the derived
/// worktree probe. A dead instrument blocks; absence after a run probe clears.
pub(crate) fn review_activity(cwd: &Path, branch: &str, pr_head: &str) -> Value {
    let activity = |blocker: &str, detail: &str, hold: Value, worktree: Value| {
        json!({
            "blocker": blocker,
            "detail": detail,
            "hold": hold,
            "worktree": worktree,
        })
    };
    if branch.is_empty() {
        return activity(
            "",
            "",
            Value::Null,
            not_reached("no head branch on the PR read"),
        );
    }
    let key = format!("review:branch:{branch}");
    let (state, record) = crate::claims::status(&key, None);
    let hold_json = || -> Value {
        record
            .as_ref()
            .map(|r| serde_json::to_value(r).unwrap_or(Value::Null))
            .unwrap_or(Value::Null)
    };
    match state {
        crate::claims::ClaimState::Corrupted => {
            return activity(
                REVIEW_HOLD_UNREADABLE,
                &format!("review hold on {branch} could not be parsed; refusing to assume unheld"),
                hold_json(),
                not_reached("not reached"),
            );
        }
        crate::claims::ClaimState::Live | crate::claims::ClaimState::Suspect => {
            let holder = record
                .as_ref()
                .map(|r| r.holder.clone())
                .unwrap_or_default();
            let head = record
                .as_ref()
                .map(|r| &r.metadata)
                .and_then(|m| m.get("head_sha"))
                .and_then(Value::as_str)
                .unwrap_or("an unrecorded head")
                .to_string();
            return activity(
                REVIEW_IN_FLIGHT,
                &format!(
                    "a review is in flight on {branch}: held by {holder} at {head}. Merging \
now ships the code the review is still fixing. Clear it with `fno do pr review-hold release --branch {branch}` once the review has landed its findings. A clean re-review clears it on its own, through the attestation."
                ),
                hold_json(),
                not_reached("not reached"),
            );
        }
        crate::claims::ClaimState::Stale => {
            let holder = record
                .as_ref()
                .map(|r| r.holder.clone())
                .unwrap_or_else(|| "unknown".into());
            eprintln!(
                "note: review hold on {branch} expired (holder {holder}); it no longer blocks. \
A review hold is a lease: it lapses on its TTL whether or not the holder session still runs."
            );
            let _ = crate::claims::release(
                &key,
                record.as_ref().map(|r| r.holder.as_str()).unwrap_or(""),
                None,
                None,
            );
        }
        crate::claims::ClaimState::Free => {}
    }
    let (blocker, detail, reading) = worktree_probe(cwd, branch, pr_head);
    let hold = if state == crate::claims::ClaimState::Stale {
        hold_json()
    } else {
        Value::Null
    };
    activity(blocker.as_deref().unwrap_or(""), &detail, hold, reading)
}

/// The derived layer: `(blocker, detail, reading)`. A failed enumeration
/// blocks; a run that matches no worktree clears.
fn worktree_probe(
    cwd: &Path,
    branch: &str,
    pr_head: &str,
) -> (Option<&'static str>, String, Value) {
    let mut reading =
        json!({"probed": false, "path": null, "dirty": null, "head": null, "note": ""});
    let Ok(listed) = std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(cwd)
        .output()
    else {
        reading["note"] = json!("git worktree list could not run");
        return (
            Some(WORKTREE_PROBE_FAILED),
            "worktree probe could not run: git worktree list could not run; refusing to assume nothing is in flight".into(),
            reading,
        );
    };
    if !listed.status.success() {
        let err = String::from_utf8_lossy(&listed.stderr);
        let note = err
            .trim()
            .lines()
            .next()
            .unwrap_or("git worktree list failed");
        reading["note"] = json!(note);
        return (
            Some(WORKTREE_PROBE_FAILED),
            format!(
                "worktree probe could not run: {note}; refusing to assume nothing is in flight"
            ),
            reading,
        );
    }
    reading["probed"] = json!(true);
    let text = String::from_utf8_lossy(&listed.stdout);
    let entries = parse_worktree_list(&text);
    let matched = entries
        .iter()
        .find(|e| e.branch.as_deref() == Some(branch) && std::path::Path::new(&e.path).is_dir());
    let Some(matched) = matched else {
        reading["note"] = json!("no local worktree on this branch");
        return (None, String::new(), reading);
    };
    reading["path"] = json!(matched.path);
    reading["head"] = json!(matched.head.clone().unwrap_or_default());
    let state_dir = std::path::Path::new(&matched.path).join(".fno");
    reading["manifest_path"] = Value::Null;
    reading["harness_session_id"] = Value::Null;
    reading["authority_note"] = json!("matched worktree; no readable target manifest");
    let mut manifests: Vec<std::path::PathBuf> = Vec::new();
    let live = state_dir.join("target-state.md");
    let live_is_file = live.is_file();
    if live_is_file {
        manifests.push(live.clone());
    }
    for pattern in [
        "target-state.terminal.*.md",
        "target-state.md.archived.*.md",
    ] {
        let mut hits: Vec<std::path::PathBuf> = std::fs::read_dir(&state_dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .map(|n| glob_match(pattern, n))
                            .unwrap_or(false)
                    })
                    .collect()
            })
            .unwrap_or_default();
        hits.sort();
        hits.reverse();
        if let Some(first) = hits.first() {
            manifests.push(first.clone());
            break;
        }
    }
    for manifest in manifests {
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        if let Some(owner) = crate::loopcheck::scan_manifest_field(&text, "harness_session_id") {
            reading["manifest_path"] = json!(manifest.display().to_string());
            reading["harness_session_id"] = json!(owner);
            reading["authority_note"] = json!(if manifest == live {
                "live target manifest"
            } else {
                "newest archived target manifest"
            });
            break;
        }
    }
    let Ok(status) = std::process::Command::new("git")
        .args([
            "-C",
            &matched.path,
            "status",
            "--porcelain",
            "--untracked-files=no",
        ])
        .current_dir(cwd)
        .output()
    else {
        reading["probed"] = json!(false);
        reading["note"] = json!("git status could not run");
        return (
            Some(WORKTREE_PROBE_FAILED),
            "worktree probe could not run: git status could not run".into(),
            reading,
        );
    };
    if !status.status.success() {
        reading["probed"] = json!(false);
        let note = String::from_utf8_lossy(&status.stderr);
        let note = note.trim().lines().next().unwrap_or("git status failed");
        reading["note"] = json!(note);
        return (
            Some(WORKTREE_PROBE_FAILED),
            format!("worktree probe could not read {}: {note}", matched.path),
            reading,
        );
    }
    let dirty = !String::from_utf8_lossy(&status.stdout).trim().is_empty();
    reading["dirty"] = json!(dirty);
    if dirty {
        return (
            Some(WORKTREE_DIRTY),
            format!(
                "{} carries uncommitted changes to tracked files; merging would ship the code without them",
                matched.path
            ),
            reading,
        );
    }
    let local_head = matched.head.clone().unwrap_or_default();
    if !pr_head.is_empty() && !local_head.is_empty() && local_head != pr_head {
        return (
            Some(WORKTREE_HEAD_MISMATCH),
            format!(
                "{} is at {local_head} but the PR would merge {pr_head}; the local branch is not what this merge lands",
                matched.path
            ),
            reading,
        );
    }
    (None, String::new(), reading)
}

struct WorktreeEntry {
    path: String,
    head: Option<String>,
    branch: Option<String>,
}

fn parse_worktree_list(text: &str) -> Vec<WorktreeEntry> {
    let mut entries: Vec<WorktreeEntry> = Vec::new();
    let mut current: Option<WorktreeEntry> = None;
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(done) = current.take() {
                entries.push(done);
            }
            current = Some(WorktreeEntry {
                path: path.to_string(),
                head: None,
                branch: None,
            });
        } else if let Some(head) = line.strip_prefix("HEAD ") {
            if let Some(c) = current.as_mut() {
                c.head = Some(head.to_string());
            }
        } else if let Some(branch) = line.strip_prefix("branch refs/heads/") {
            if let Some(c) = current.as_mut() {
                c.branch = Some(branch.to_string());
            }
        }
    }
    if let Some(done) = current.take() {
        entries.push(done);
    }
    entries
}

/// `fnmatch`-lite for the archived-manifest patterns; the only glob
/// metachar the names carry is `*`.
fn glob_match(pattern: &str, name: &str) -> bool {
    fn inner(p: &[u8], n: &[u8]) -> bool {
        match (p.first(), n.first()) {
            (None, None) => true,
            (Some(b'*'), _) => inner(&p[1..], n) || (!n.is_empty() && inner(p, &n[1..])),
            (Some(a), Some(b)) if a == b => inner(&p[1..], &n[1..]),
            _ => false,
        }
    }
    inner(pattern.as_bytes(), name.as_bytes())
}

/// The owner-guidance fact: a counted local review authored by another
/// session, with the authority note the manifest state yields.
pub(crate) fn owner_guidance(coverage: &Value, worktree: &Value) -> Option<Value> {
    let verdicts = coverage.get("verdicts").and_then(Value::as_array)?;
    let counted_other_session = verdicts.iter().any(|v| {
        v.get("producer").and_then(Value::as_str) == Some("local_attestation")
            && v.get("name").and_then(Value::as_str) == Some("code-review")
            && v.get("verdict").and_then(Value::as_str) == Some("reviewed")
            && counted_freshness(v.get("freshness").unwrap_or(&Value::Null))
            && v.get("attestation_origin").and_then(Value::as_str) == Some("other_session")
    });
    if !counted_other_session {
        return None;
    }
    let raw_owner = coverage.get("author_session_id").and_then(Value::as_str);
    let event_owner = raw_owner.filter(|o| !o.is_empty());
    let live_owner = worktree.get("harness_session_id").and_then(Value::as_str);
    let authority_note = if event_owner.is_some() && live_owner == event_owner {
        format!(
            "{}; matches coverage event author",
            worktree
                .get("authority_note")
                .and_then(Value::as_str)
                .unwrap_or("target manifest")
        )
    } else if event_owner.is_some() {
        "coverage event author; current worktree manifest owner differs".to_string()
    } else {
        "coverage event lacks author_session_id; current manifest is not historical evidence"
            .to_string()
    };
    Some(json!({
        "attestation_origin": "other_session",
        "counts": true,
        "harness_session_id": event_owner,
        "worktree_path": worktree.get("path").cloned().unwrap_or(Value::Null),
        "manifest_path": worktree.get("manifest_path").cloned().unwrap_or(Value::Null),
        "authority_note": authority_note,
    }))
}

/// The coverage-status repost: published through the hidden
/// `coverage-publish` verb, so the POSTed verdict stays the gate's own
/// answer, never a re-derivation.
pub(crate) fn republish_coverage_status(cwd: &Path, pr: u64, head: &str) -> (bool, String) {
    let mut cmd = std::process::Command::new("fno");
    cmd.args(["do", "pr", "coverage-publish", &pr.to_string()]);
    if !head.is_empty() {
        cmd.args(["--head", head]);
    }
    cmd.current_dir(cwd);
    let Ok(out) = cmd.output() else {
        return (false, "coverage-publish could not run".into());
    };
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    match serde_json::from_str::<Value>(text.trim()) {
        Ok(v) => (
            v.get("posted").and_then(Value::as_bool).unwrap_or(false),
            v.get("note")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        ),
        Err(_) => (out.status.success(), String::new()),
    }
}
