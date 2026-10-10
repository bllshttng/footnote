//! Native triage health + trend: the aggregate report, threshold
//! evaluation, notification dispatch, history, and the rolling trend of
//! `fno/graph/triage.py::cmd_health` / `cmd_trend`, over the native
//! owners it already has (collision, rollup, batch, evals fold).

use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;

use super::triage_cli::echo_json;

/// The repo-root canonical events journal the advisory folds read
/// (triage.py _events_path): anchored to the git toplevel so producer and
/// consumer coincide from any subdirectory.
fn canonical_events_path() -> Option<PathBuf> {
    let root = super::triage::intake_repo_root()?;
    Some(root.join(".fno").join("events.jsonl"))
}

/// Tolerant read of the canonical `{ts,type,source,data}` events log:
/// a malformed line is skipped, never raised - the health fold is
/// advisory and must not break because one event row is corrupt.
pub(crate) fn read_canonical_events() -> Vec<Value> {
    let Some(path) = canonical_events_path() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect()
}

/// `(node, pr refs)` candidates for the done-not-merged invariant within
/// the 7-day window (triage.py DONE_NOT_MERGED_WINDOW_DAYS).
const DONE_NOT_MERGED_WINDOW_DAYS: i64 = 7;

/// Every `(repo, number)` PR ref a node carries: the primary pr_number on
/// its repo slug, then one ref per additional_prs row (triage.py's
/// node_pr_refs). The repo slug parses from the URL; an unparseable URL
/// leaves the repo empty so the state read marks it unknown.
fn node_pr_refs(node: &Value) -> Vec<(String, i64, String)> {
    let mut out = Vec::new();
    if let Some(num) = node.get("pr_number").and_then(Value::as_i64) {
        let url = node
            .get("pr_url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        out.push((repo_slug_from_url(&url), num, url));
    }
    if let Some(extras) = node.get("additional_prs").and_then(Value::as_array) {
        for extra in extras {
            let num = match extra {
                Value::Number(n) => n.as_i64(),
                Value::Object(o) => o.get("pr_number").and_then(Value::as_i64),
                _ => None,
            };
            let Some(num) = num else { continue };
            let url = match extra {
                Value::String(s) => s.clone(),
                Value::Object(o) => o
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                _ => String::new(),
            };
            out.push((repo_slug_from_url(&url), num, url));
        }
    }
    out
}

/// `owner/name` out of a git remote URL; empty when unparseable.
pub(crate) fn repo_slug_from_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches(".git").trim_end_matches('/');
    let after_scheme = trimmed
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(trimmed);
    let after_host = match after_scheme.split_once('/') {
        Some((_, rest)) => rest,
        None => {
            let rest = after_scheme.strip_prefix(':').unwrap_or(after_scheme);
            rest
        }
    };
    let mut parts: Vec<&str> = after_host.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() < 2 {
        return String::new();
    }
    let name = parts.pop().unwrap_or("");
    let owner = parts.pop().unwrap_or("");
    format!("{owner}/{name}")
}

/// `(repo, pr_number) -> gh state` for a batch of refs, ONE `gh pr list`
/// per repo (triage.py _pr_states_by_repo). A repo that could not be read
/// contributes no states and lands in the outage list, so its nodes read
/// unknown rather than as violations.
fn pr_states_by_repo(refs: &[(String, i64)]) -> (BTreeMap<(String, i64), String>, Vec<String>) {
    let mut states = BTreeMap::new();
    let mut outages = Vec::new();
    let mut repos: Vec<String> = refs
        .iter()
        .map(|(repo, _)| repo.clone())
        .filter(|r| !r.is_empty())
        .collect();
    repos.sort();
    repos.dedup();
    if repos.is_empty() {
        return (states, outages);
    }
    let gh_ok = std::process::Command::new("gh")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !gh_ok {
        return (states, repos);
    }
    for repo in repos {
        let out = crate::bounded_cmd::output_with_timeout_result(
            {
                let mut c = std::process::Command::new("gh");
                c.args([
                    "pr",
                    "list",
                    "--state",
                    "all",
                    "--limit",
                    "200",
                    "--repo",
                    &repo,
                    "--json",
                    "number,state",
                ]);
                c
            },
            30,
        );
        let Ok(out) = out else {
            outages.push(repo);
            continue;
        };
        if !out.status.success() {
            outages.push(repo);
            continue;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let Ok(rows) = serde_json::from_str::<Value>(text.trim()) else {
            outages.push(repo);
            continue;
        };
        if let Some(rows) = rows.as_array() {
            for row in rows {
                if let (Some(num), Some(state)) = (
                    row.get("number").and_then(Value::as_i64),
                    row.get("state").and_then(Value::as_str),
                ) {
                    states.insert((repo.clone(), num), state.to_string());
                }
            }
        }
    }
    (states, outages)
}

/// `(node_id, pr_number|None)` pairs carrying a backlog_done_forced
/// receipt (triage.py _forced_close_receipts). `roots` names extra
/// project journals a foreign-scope check must read.
fn forced_close_receipts(roots: &[PathBuf]) -> std::collections::BTreeSet<(String, Option<i64>)> {
    let mut pairs = std::collections::BTreeSet::new();
    let mut paths: Vec<PathBuf> = Vec::new();
    for r in roots {
        paths.push(r.join(".fno").join("events.jsonl"));
    }
    if let Some(p) = canonical_events_path() {
        paths.push(p);
    }
    for path in paths {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in text.lines() {
            if !line.contains("backlog_done_forced") {
                continue;
            }
            let Ok(ev) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let body = ev
                .get("data")
                .or_else(|| ev.get("payload"))
                .cloned()
                .unwrap_or_else(|| ev.clone());
            let Some(nid) = body
                .get("node_id")
                .and_then(Value::as_str)
                .or_else(|| ev.get("node_id").and_then(Value::as_str))
            else {
                continue;
            };
            let pr = body.get("pr_number").and_then(Value::as_i64);
            pairs.insert((nid.to_string(), pr));
        }
    }
    pairs
}

/// The done=merged invariant report (triage.py done_not_merged_report):
/// nodes closed inside the window over a PR gh reports not merged, with
/// no forced-close receipt for the closure. Unreadable refs read
/// unknown, never violations.
pub(crate) fn done_not_merged_report(entries: &[Value], roots: &[PathBuf]) -> Value {
    let now_ms = super::advance::now_ms_i64();
    let cutoff_ms = now_ms - DONE_NOT_MERGED_WINDOW_DAYS * 86_400_000;
    let mut candidates: Vec<(Value, Vec<(String, i64, String)>)> = Vec::new();
    for e in entries {
        let Some(completed) = e.get("completed_at").and_then(Value::as_str) else {
            continue;
        };
        let Some(pr_number) = e.get("pr_number").and_then(Value::as_i64) else {
            continue;
        };
        let Some(closed_ms) = crate::event_store::parse_rfc3339_ms(completed) else {
            continue;
        };
        if closed_ms < cutoff_ms {
            continue;
        }
        candidates.push((e.clone(), node_pr_refs(e)));
    }
    if candidates.is_empty() {
        return json!({ "violations": [], "unknown": [], "checked": 0, "window_days": DONE_NOT_MERGED_WINDOW_DAYS });
    }
    let all_pairs: Vec<(String, i64)> = candidates
        .iter()
        .flat_map(|(_, refs)| refs.iter().map(|(r, n, _)| (r.clone(), *n)))
        .collect();
    let (states, outage_repos) = pr_states_by_repo(&all_pairs);
    let mut roots_seen: Vec<PathBuf> = Vec::new();
    for (e, _) in &candidates {
        let cwd = e
            .get("_resolved_cwd")
            .or_else(|| e.get("cwd"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        if let Some(cwd) = cwd {
            let p = PathBuf::from(super::super::territory::normalize_path(cwd));
            if !roots_seen.contains(&p) {
                roots_seen.push(p);
            }
        }
    }
    for r in roots {
        if !roots_seen.contains(r) {
            roots_seen.push(r.clone());
        }
    }
    let forced = forced_close_receipts(&roots_seen);
    let mut violations = Vec::new();
    let mut unknown = Vec::new();
    for (node, refs) in &candidates {
        let nid = node.get("id").and_then(Value::as_str).unwrap_or("");
        let primary_num = refs.first().map(|(_, n, _)| *n).unwrap_or(0);
        let record = json!({
            "id": node.get("id"),
            "title": node.get("title"),
            "pr_number": primary_num,
            "completed_at": node.get("completed_at"),
        });
        if forced.contains(&(nid.to_string(), None))
            || refs
                .iter()
                .any(|(_, num, _)| forced.contains(&(nid.to_string(), Some(*num))))
        {
            continue;
        }
        let ref_states: Vec<Option<&String>> = refs
            .iter()
            .map(|(repo, num, _)| states.get(&(repo.clone(), *num)))
            .collect();
        if ref_states.iter().any(|s| *s == Some(&"MERGED".to_string())) {
            continue;
        }
        if ref_states.iter().any(|s| s.is_none()) {
            let mut reason = "unknown".to_string();
            for ((repo, _, _), st) in refs.iter().zip(ref_states.iter()) {
                if st.is_none() {
                    if repo.is_empty() {
                        reason = "no pr_url to resolve the repo".to_string();
                    } else if outage_repos.contains(repo) {
                        reason = "gh outage".to_string();
                    } else {
                        reason = "not in gh window".to_string();
                    }
                    break;
                }
            }
            let mut row = record.clone();
            row["reason"] = json!(reason);
            unknown.push(row);
            continue;
        }
        let mut row = record.clone();
        row["pr_state"] = json!(ref_states[0].unwrap_or(&String::new()));
        violations.push(row);
    }
    json!({
        "violations": violations,
        "unknown": unknown,
        "checked": candidates.len(),
        "window_days": DONE_NOT_MERGED_WINDOW_DAYS,
    })
}

/// The auto-failure sentinel (fno.graph.failure.AUTO_FAILURE_SENTINEL): a
/// hand-deferred node never strand-reports its dependents.
pub(crate) const AUTO_FAILURE_SENTINEL: &str = "auto-failure:";

/// Deferred with the auto-failure sentinel reason (failure.py
/// is_auto_failure_deferred).
fn is_auto_failure_deferred(e: &Value) -> bool {
    if e.get("deferred_at").map(Value::is_null).unwrap_or(true) {
        return false;
    }
    e.get("deferred_reason")
        .and_then(Value::as_str)
        .map(|r| r.starts_with(AUTO_FAILURE_SENTINEL))
        .unwrap_or(false)
}

/// Auto-failure-deferred blockers -> their dependents (failure.py
/// stranded_dependents): read-only surfacing; a blocker with no
/// dependents is omitted, an absent entry means nothing stranded.
pub(crate) fn stranded_dependents(entries: &[Value]) -> BTreeMap<String, Vec<String>> {
    let blockers: Vec<String> = entries
        .iter()
        .filter(|e| is_auto_failure_deferred(e))
        .filter_map(|e| e.get("id").and_then(Value::as_str).map(str::to_string))
        .collect();
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for e in entries {
        let Some(eid) = e.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(blocked) = e.get("blocked_by").and_then(Value::as_array) else {
            continue;
        };
        for bid in blocked.iter().filter_map(Value::as_str) {
            if blockers.iter().any(|b| b == bid) {
                out.entry(bid.to_string())
                    .or_default()
                    .push(eid.to_string());
            }
        }
    }
    out
}

/// OPEN feature/task work with no epic ancestor and no opt-out (rollup.py
/// is_orphan); the ancestry walk reaches through the full parent chain
/// with a seen-set bounding a malformed cycle.
fn is_orphan(entry: &Value, index: &BTreeMap<String, &Value>) -> bool {
    if !matches!(
        entry.get("type").and_then(Value::as_str),
        Some("feature") | Some("task")
    ) {
        return false;
    }
    if entry
        .get("orphan_ok")
        .map(|v| !v.is_null() && v != &Value::Bool(false))
        .unwrap_or(false)
    {
        return false;
    }
    if matches!(
        entry.get("status").and_then(Value::as_str),
        Some("done") | Some("superseded") | Some("deferred")
    ) {
        return false;
    }
    let mut seen: Vec<String> = Vec::new();
    let mut current = entry
        .get("parent")
        .and_then(Value::as_str)
        .map(str::to_string);
    while let Some(pid) = current {
        if seen.contains(&pid) {
            break;
        }
        seen.push(pid.clone());
        let Some(parent) = index.get(&pid) else {
            return false;
        };
        if parent.get("type").and_then(Value::as_str) == Some("epic") {
            return true;
        }
        current = parent
            .get("parent")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    false
}

/// The orphan fold (triage.py section 10): every open feature/task with no
/// mission edge, over the UNFILTERED index; the rate is project-scoped.
pub(crate) fn orphan_fold(entries: &[Value], all_entries: &[Value]) -> (Option<f64>, Vec<String>) {
    let index: BTreeMap<String, &Value> = all_entries
        .iter()
        .filter_map(|e| {
            e.get("id")
                .and_then(Value::as_str)
                .map(|id| (id.to_string(), e))
        })
        .collect();
    let non_exempt: Vec<&Value> = entries
        .iter()
        .filter(|e| {
            matches!(
                e.get("type").and_then(Value::as_str),
                Some("feature") | Some("task")
            ) && !e
                .get("orphan_ok")
                .map(|v| !v.is_null() && v != &Value::Bool(false))
                .unwrap_or(false)
                && !matches!(
                    e.get("status").and_then(Value::as_str),
                    Some("done") | Some("superseded") | Some("deferred")
                )
        })
        .collect();
    let orphan_nodes: Vec<String> = non_exempt
        .iter()
        .filter_map(|e| e.get("id").and_then(Value::as_str))
        .filter(|id| {
            e_str_map_lookup(entries, id)
                .map(|e| is_orphan(e, &index))
                .unwrap_or(false)
        })
        .map(str::to_string)
        .collect();
    if non_exempt.is_empty() {
        return (Some(0.0), orphan_nodes);
    }
    let rate = orphan_nodes.len() as f64 / non_exempt.len() as f64;
    let rounded = (rate * 10000.0).round() / 10000.0;
    (Some(rounded), orphan_nodes)
}

fn e_str_map_lookup<'a>(entries: &'a [Value], id: &str) -> Option<&'a Value> {
    entries
        .iter()
        .find(|e| e.get("id").and_then(Value::as_str) == Some(id))
}

/// Batch-lane verdict (batch.py read_batch_events + compute_metrics):
/// ship rows earn members-1 saved runs, abandon rows waste members; the
/// verdict names build-wave4 or disable-batching only when it says act.
pub(crate) fn batch_verdict(events_path: &PathBuf) -> Option<String> {
    let text = std::fs::read_to_string(events_path).ok()?;
    let mut shipped = 0i64;
    let mut abandoned = 0i64;
    let mut saved = 0i64;
    let mut wasted = 0i64;
    for line in text.lines() {
        let Ok(ev) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let data = ev.get("data").cloned().unwrap_or(Value::Null);
        match ev.get("type").and_then(Value::as_str) {
            Some("active_backlog_batch_ship") => {
                let stdout = data.get("stdout").and_then(Value::as_str).unwrap_or("{}");
                let parsed: Value = serde_json::from_str(stdout).unwrap_or(json!({}));
                for r in parsed
                    .get("shipped")
                    .and_then(Value::as_array)
                    .unwrap_or(&vec![])
                {
                    let members = r
                        .get("members")
                        .and_then(Value::as_array)
                        .map(Vec::len)
                        .unwrap_or(0) as i64;
                    match r.get("action").and_then(Value::as_str) {
                        Some("shipped") => {
                            shipped += 1;
                            saved += (members - 1).max(0);
                        }
                        Some("abandoned") => {
                            abandoned += 1;
                            wasted += members.max(0);
                        }
                        _ => {}
                    }
                }
            }
            Some("active_backlog_batch_abandon") => {
                if data.get("detail").and_then(Value::as_str) != Some("ok") {
                    continue;
                }
                abandoned += 1;
                wasted += data
                    .get("member_count")
                    .and_then(Value::as_i64)
                    .unwrap_or(0)
                    .max(0);
            }
            _ => {}
        }
    }
    verdict_of(shipped, abandoned, saved, wasted)
}

/// The binary economic comparison (batch.py _verdict).
fn verdict_of(shipped: i64, abandoned: i64, saved: i64, wasted: i64) -> Option<String> {
    let opened = shipped + abandoned;
    if opened == 0 {
        return None;
    }
    let net = saved - wasted;
    let abandon_rate = abandoned as f64 / opened as f64;
    if net < 0 && abandon_rate > 0.5 {
        return Some("disable-batching".to_string());
    }
    if wasted > 0 && (net <= 0 || wasted as f64 > 0.4 * saved as f64) {
        return Some("build-wave4".to_string());
    }
    Some("keep-v1".to_string())
}

/// The evals section payload (evals/report.py evals_health_summary over
/// the native fold): None when the history is absent or has no rows.
pub(crate) fn evals_summary() -> Option<Value> {
    let path = evals_history_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    let stale_days: i64 = 7;
    let payload = crate::evals_trend::summary_payload(&text, stale_days, chrono::Utc::now());
    if payload
        .get("row_count")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        == 0
    {
        return None;
    }
    Some(json!({
        "regression_pass_rate": payload.get("regression_pass_rate"),
        "flake_count": payload.get("flake_count"),
        "regression_alarm": payload.get("regression_alarm"),
        "regressed": payload.get("regressed"),
        "window_days": stale_days,
        "age_days": payload.get("age_days"),
        "stale": payload.get("stale"),
        "never_ran": payload.get("never_ran"),
    }))
}

/// The evals history file (fno.paths.evals_history).
fn evals_history_path() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    if let Some(pin) = std::env::var_os("FNO_EVALS_HISTORY").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(pin));
    }
    let root = super::triage::intake_repo_root()?;
    Some(root.join(".fno").join("evals").join("history.jsonl"))
}

// ---------------------------------------------------------------------------
// Threshold evaluation (health_monitor.py evaluate_thresholds).
// ---------------------------------------------------------------------------

/// The merged `config.health_monitor` read (health_monitor.py load_config):
/// every key the model carries is present; a broken config degrades to the
/// defaults with one stderr line.
pub(crate) fn hm_config() -> Value {
    let doc = super::advance_settings::load_merged(None);
    let block = doc.get("health_monitor").cloned().unwrap_or(Value::Null);
    let mut base = json!({
        "enabled": true,
        "thresholds": {
            "idea_pile_depth": 25,
            "stale_ready_days": 30,
            "failure_prone_attempts": 2,
       "collision_count": 3,
            "project_cwd_mismatch": 0,
            "orphan_feature_rate": 1.0,
        },
        "notifications": { "surfaces": ["terminal"], "throttle_minutes": 60 },
        "history": { "enabled": true, "retain_days": 90 },
    });
    if let Some(b) = block.as_object() {
        deep_merge_health(&mut base, Value::Object(b.clone()));
    }
    base
}

fn deep_merge_health(base: &mut Value, over: Value) {
    match (base, over) {
        (Value::Object(base), Value::Object(over)) => {
            for (k, v) in over {
                match base.get_mut(&k) {
                    Some(slot) if slot.is_object() && v.is_object() => {
                        deep_merge_health(slot, v);
                    }
                    _ => {
                        base.insert(k, v);
                    }
                }
            }
        }
        _ => {}
    }
}

/// A threshold's configured value or its model default (the _sanitize
/// ratchet drops junk to the default).
fn thresh_i(config: &Value, key: &str, default: i64) -> i64 {
    config["thresholds"][key]
        .as_i64()
        .or_else(|| config["thresholds"][key].as_f64().map(|f| f as i64))
        .unwrap_or(default)
}

fn thresh_f(config: &Value, key: &str, default: f64) -> f64 {
    config["thresholds"][key].as_f64().unwrap_or(default)
}

/// A breach record (health_monitor.py Breach.to_jsonable).
fn breach(key: &str, actual: f64, threshold: f64, kind: &str, hint: &str) -> Value {
    let severity = classify_severity(actual, threshold, kind);
    let mut msg = format!("{key}: actual={actual}, threshold={threshold}");
    if !hint.is_empty() {
        msg.push_str(&format!(" ({hint})"));
    }
    json!({
        "key": key,
        "actual": actual,
        "threshold": threshold,
        "severity": severity,
        "message": msg,
    })
}

/// Severity from overshoot magnitude (health_monitor.py
/// _classify_severity): count ratio brackets, presence = absolute count,
/// rate = headroom consumed.
fn classify_severity(actual: f64, threshold: f64, kind: &str) -> &'static str {
    let ratio = match kind {
        "rate" => {
            let headroom = 1.0 - threshold;
            let consumed = if headroom <= 0.0 {
                1.0
            } else {
                (actual - threshold) / headroom
            };
            return if consumed >= 0.5 {
                "alert"
            } else if consumed >= 0.25 {
                "warn"
            } else {
                "info"
            };
        }
        "count" => {
            if threshold <= 0.0 {
                5.0
            } else {
                actual / threshold
            }
        }
        _ => actual,
    };
    if ratio >= 5.0 {
        "alert"
    } else if ratio >= 2.0 {
        "warn"
    } else {
        "info"
    }
}

/// The nine threshold keys (health_monitor.py evaluate_thresholds): an
/// empty list means all green; `enabled: false` short-circuits to green.
pub(crate) fn evaluate_thresholds(report: &Value, config: &Value) -> Vec<Value> {
    if config.get("enabled").and_then(Value::as_bool) == Some(false) {
        return Vec::new();
    }
    let mut breaches = Vec::new();
    let idea = report["idea_pile_depth"].as_i64().unwrap_or(0);
    let idea_t = thresh_i(config, "idea_pile_depth", 25);
    if idea > idea_t {
        breaches.push(breach(
            "idea_pile_depth",
            idea as f64,
            idea_t as f64,
            "count",
            "idea-status nodes accumulating",
        ));
    }
    let stale_n = report["stale_ready_nodes"]
        .as_array()
        .map(Vec::len)
        .unwrap_or(0) as f64;
    if stale_n > 0.0 {
        let days = thresh_i(config, "stale_ready_days", 30);
        breaches.push(breach(
            "stale_ready_nodes",
            stale_n,
            days as f64,
            "presence",
            &format!("ready nodes older than {days}d"),
        ));
    }
    let fp_n = report["failure_prone_nodes"]
        .as_array()
        .map(Vec::len)
        .unwrap_or(0) as f64;
    if fp_n > 0.0 {
        let attempts = thresh_i(config, "failure_prone_attempts", 2);
        breaches.push(breach(
            "failure_prone_nodes",
            fp_n,
            attempts as f64,
            "presence",
            &format!(">={attempts} attempts, no PR"),
        ));
    }
    let coll_n = report["collisions"].as_array().map(Vec::len).unwrap_or(0) as f64;
    let coll_t = thresh_i(config, "collision_count", 3);
    if coll_n > coll_t as f64 {
        breaches.push(breach(
            "collisions",
            coll_n,
            coll_t as f64,
            "count",
            "medium+ collisions",
        ));
    }
    let mis_n = report["project_cwd_mismatch"].as_i64().unwrap_or(0) as f64;
    let mis_t = thresh_i(config, "project_cwd_mismatch", 0);
    if mis_n > mis_t as f64 {
        breaches.push(breach(
            "project_cwd_mismatch",
            mis_n,
            mis_t as f64,
            "count",
            "project/cwd disagree on pending nodes; producer regression?",
        ));
    }
    if let Some(rate) = report.get("orphan_feature_rate").and_then(Value::as_f64) {
        let rate_t = thresh_f(config, "orphan_feature_rate", 1.0);
        if rate > rate_t {
            breaches.push(breach(
                "orphan_feature_rate",
                rate,
                rate_t,
                "rate",
                "open features that resolve no mission edge",
            ));
        }
    }
    let dnm_n = report["done_not_merged"]
        .as_array()
        .map(Vec::len)
        .unwrap_or(0) as f64;
    if dnm_n > 0.0 {
        breaches.push(breach(
            "done_not_merged",
            dnm_n,
            0.0,
            "presence",
            "closed over an unmerged PR",
        ));
    }
    let su_n = report["supersession_unverified"]
        .as_array()
        .map(Vec::len)
        .unwrap_or(0) as f64;
    if su_n > 0.0 {
        breaches.push(breach("supersession_unverified", su_n, 0.0, "presence", "successor merged but declared surfaces were not in its PR. Reopen with fno backlog unsupersede <id>, or accept with fno backlog done <id>; the sweep will never settle it alone."));
    }
    let bh_n = report["blocked_by_held"]
        .as_array()
        .map(Vec::len)
        .unwrap_or(0) as f64;
    if bh_n > 0.0 {
        breaches.push(breach("blocked_by_held", bh_n, 0.0, "presence", "a deferred or missing blocker holds this node; undefer the blocker, drop the edge with fno backlog update <node> --blocked-by ..., or defer the dependent too."));
    }
    breaches
}

// ---------------------------------------------------------------------------
// Notification dispatch (health_monitor.py dispatch_notifications).
// ---------------------------------------------------------------------------

/// The throttle-state file (health_monitor.py _default_throttle_path).
fn throttle_path() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    crate::agents_config::state_dir(&cwd)
        .map(|root| root.join("state").join("health-throttle.json"))
}

/// The alert log (health_monitor.py _default_alert_log_path).
fn alert_log_path() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    crate::agents_config::state_dir(&cwd).map(|root| root.join("logs").join("health-alerts.log"))
}

/// The breach line the terminal and log surfaces share.
fn breach_line(b: &Value) -> String {
    format!(
        "[{}] {}: actual={}, threshold={} - {}",
        b["severity"].as_str().unwrap_or("").to_uppercase(),
        b["key"].as_str().unwrap_or(""),
        b["actual"],
        b["threshold"],
        b["message"].as_str().unwrap_or("")
    )
}

/// Breach notifications to the configured surfaces (health_monitor.py
/// dispatch_notifications): throttle suppresses non-alert keys that fired
/// within the window, surfaces run in config order, and log_only is the
/// last-resort recorder so a misconfigured webhook never loses the breach.
pub(crate) fn dispatch_notifications(report: &Value, breaches: &[Value], config: &Value) {
    if breaches.is_empty() {
        return;
    }
    if config.get("enabled").and_then(Value::as_bool) == Some(false) {
        return;
    }
    let notif = config.get("notifications").cloned().unwrap_or(json!({}));
    let surfaces: Vec<String> = notif
        .get("surfaces")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .filter(|rows: &Vec<String>| !rows.is_empty())
        .unwrap_or_else(|| vec!["terminal".to_string()]);
    let throttle_minutes = notif
        .get("throttle_minutes")
        .and_then(Value::as_i64)
        .unwrap_or(60);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let throttle_file = throttle_path();
    let mut state: BTreeMap<String, String> = throttle_file
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|v| {
            v.as_object().map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
        })
        .unwrap_or_default();
    let mut fresh: Vec<Value> = Vec::new();
    for b in breaches {
        if b["severity"].as_str() == Some("alert") {
            fresh.push(b.clone());
            continue;
        }
        if throttle_minutes <= 0 {
            fresh.push(b.clone());
            continue;
        }
        let last = state.get(b["key"].as_str().unwrap_or(""));
        let Some(last) = last else {
            fresh.push(b.clone());
            continue;
        };
        let Some(last_ms) = crate::event_store::parse_rfc3339_ms(last) else {
            fresh.push(b.clone());
            continue;
        };
        if now * 1000 - last_ms >= throttle_minutes * 60 * 1000 {
            fresh.push(b.clone());
            continue;
        }
    }
    if fresh.is_empty() {
        return;
    }
    let mut dispatched_any = false;
    for surface in &surfaces {
        match surface.as_str() {
            "terminal" => {
                eprintln!(
                    "Backlog health breach in {}:",
                    report["scope"].as_str().unwrap_or("")
                );
                for b in &fresh {
                    eprintln!("  {}", breach_line(b));
                }
                dispatched_any = true;
            }
            "log_only" => {
                log_breaches(&fresh, report);
                dispatched_any = true;
            }
            "webhook" | "discord" => {
                let key = if surface == "webhook" {
                    "webhook_url"
                } else {
                    "discord_channel"
                };
                let Some(url) = notif.get(key).and_then(Value::as_str) else {
                    eprintln!("health_monitor: {surface} surface requested but {key} is unset");
                    continue;
                };
                if url.starts_with("https://")
                    && post_webhook(&fresh, report, url, surface == "discord")
                {
                    dispatched_any = true;
                } else if !url.starts_with("https://") {
                    eprintln!(
                        "health_monitor: discord surface configured with channel {url:?} but no helper is wired; configure a Discord webhook URL or use the webhook surface instead. Falling through."
                    );
                }
            }
            other => {
                eprintln!("health_monitor: unknown notification surface {other:?}");
            }
        }
    }
    if !dispatched_any {
        log_breaches(&fresh, report);
    }
    for b in &fresh {
        if b["severity"].as_str() == Some("alert") {
            continue;
        }
        state.insert(
            b["key"].as_str().unwrap_or("").to_string(),
            crate::provider_cap::epoch_to_rfc3339(now),
        );
    }
    if let Some(p) = throttle_file {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let payload = serde_json::to_string_pretty(&state).unwrap_or_default();
        let tmp = p.with_extension("json.tmp");
        if std::fs::write(&tmp, payload).is_ok() {
            std::fs::rename(&tmp, &p).ok();
        }
    }
}

/// The log_only surface: one line per breach into the alert log.
fn log_breaches(breaches: &[Value], report: &Value) {
    let Some(path) = alert_log_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let ts = crate::provider_cap::epoch_to_rfc3339(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    );
    let mut body = format!("{ts}\t{}\n", report["scope"].as_str().unwrap_or(""));
    for b in breaches {
        body.push_str(&format!("\t{}\n", breach_line(b)));
    }
    use std::io::Write as _;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = f.write_all(body.as_bytes());
    }
}

/// The webhook/discord POST via bounded curl (the Python surface used
/// urllib; the crate has no HTTP client, so the same external-binary
/// posture as gh/claude applies). True on a 2xx.
fn post_webhook(breaches: &[Value], report: &Value, url: &str, discord: bool) -> bool {
    let payload = if discord {
        let mut lines = vec![format!(
            "**Backlog health breach** ({})",
            report["scope"].as_str().unwrap_or("")
        )];
        for b in breaches {
            lines.push(format!("- {}", breach_line(b)));
        }
        json!({ "content": lines.join("\n") }).to_string()
    } else {
        json!({
            "scope": report["scope"],
            "breaches": breaches,
            "report_summary": report.get("totals"),
            "timestamp": crate::provider_cap::epoch_to_rfc3339(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0)
            ),
        })
        .to_string()
    };
    let out = crate::bounded_cmd::output_with_timeout_result(
        {
            let mut c = std::process::Command::new("curl");
            c.args([
                "-sS",
                "-X",
                "POST",
                "-H",
                "Content-Type: application/json",
                "--max-time",
                "10",
                "--data",
                &payload,
                url,
            ]);
            c
        },
        15,
    );
    out.map(|o| o.status.success()).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// History log + trend (health_monitor.py append_history / read_history /
// summarize_trend, consumed by the trend verb).
// ---------------------------------------------------------------------------

/// The history file: FNO_HEALTH_HISTORY, else config.health_monitor.history.
/// path, else `<state>/history/health-history.jsonl` (triage.py cmd_health).
pub(crate) fn history_path(config: &Value) -> Option<PathBuf> {
    if let Some(pin) = std::env::var_os("FNO_HEALTH_HISTORY").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(pin));
    }
    let configured = config["history"]["path"].as_str().unwrap_or("");
    if !configured.is_empty() {
        return Some(PathBuf::from(configured));
    }
    let cwd = std::env::current_dir().ok()?;
    crate::agents_config::state_dir(&cwd)
        .map(|root| root.join("history").join("health-history.jsonl"))
}

/// Append one JSONL summary and prune past retain_days (health_monitor.py
/// append_history): healthy and breach states both log; malformed lines
/// drop with a warning; an unreadable file never gets clobbered.
pub(crate) fn append_history(report: &Value, breaches: &[Value], path: &PathBuf, retain_days: i64) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let now_ms = super::advance::now_ms_i64();
    let cutoff = now_ms - retain_days * 86_400_000;
    let mut surviving: Vec<Value> = Vec::new();
    let mut corrupt = false;
    if let Ok(text) = std::fs::read_to_string(path) {
        let mut malformed = 0usize;
        for line in text.lines() {
            let stripped = line.trim();
            if stripped.is_empty() {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<Value>(stripped) else {
                malformed += 1;
                continue;
            };
            let ts = entry
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(crate::event_store::parse_rfc3339_ms);
            let Some(ts) = ts else {
                malformed += 1;
                continue;
            };
            if ts >= cutoff {
                surviving.push(entry);
            }
        }
        if malformed > 0 {
            eprintln!(
                "health_monitor: dropped {malformed} malformed line(s) from {}",
                path.display()
            );
            if surviving.is_empty() && !text.trim().is_empty() {
                corrupt = true;
            }
        }
        if corrupt {
            let stamp = now_ms / 1000;
            let corrupt_path = path.with_extension(format!("jsonl.corrupt.{stamp}"));
            std::fs::write(&corrupt_path, &text).ok();
            eprintln!(
                "health_monitor: preserved unparseable history at {}",
                corrupt_path.display()
            );
        }
    }
    let entry = json!({
        "timestamp": crate::provider_cap::epoch_to_rfc3339(now_ms / 1000),
        "scope": report["scope"],
        "report": report,
        "breaches": breaches,
    });
    surviving.push(entry);
    let body: String = surviving
        .iter()
        .map(|e| serde_json::to_string(e).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    let tmp = path.with_extension("jsonl.tmp");
    if std::fs::write(&tmp, body + "\n").is_ok() {
        std::fs::rename(&tmp, path).ok();
    } else {
        eprintln!(
            "health_monitor: could not write history to {}",
            path.display()
        );
    }
}

/// History entries from the last `days` days, newest first (health_monitor.py
/// read_history): malformed lines skip, missing file yields empty.
pub(crate) fn read_history(path: &PathBuf, days: i64) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let cutoff = super::advance::now_ms_i64() - days * 86_400_000;
    let mut out: Vec<(String, Value)> = Vec::new();
    for line in text.lines() {
        let stripped = line.trim();
        if stripped.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(stripped) else {
            continue;
        };
        let ts = entry
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(crate::event_store::parse_rfc3339_ms);
        let Some(ts) = ts else {
            continue;
        };
        if ts >= cutoff {
            out.push((
                entry
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                entry,
            ));
        }
    }
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out.into_iter().map(|(_, e)| e).collect()
}

/// First-vs-latest deltas for the headline metrics (health_monitor.py
/// summarize_trend); empty input yields empty.
pub(crate) fn summarize_trend(entries: &[Value]) -> Value {
    let live: Vec<&Value> = entries
        .iter()
        .filter(|e| e["report"]["complete"].as_bool() != Some(false))
        .collect();
    if live.is_empty() {
        return json!({});
    }
    let first = &live[live.len() - 1]["report"];
    let latest = &live[0]["report"];
    let count_or_len = |report: &Value, key: &str| -> i64 {
        let v = report.get(key).unwrap_or(&Value::Null);
        if let Some(rows) = v.as_array() {
            return rows.len() as i64;
        }
        v.as_i64().unwrap_or(0)
    };
    let mut summary = Map::new();
    for (label, key) in [
        ("idea_pile_depth", "idea_pile_depth"),
        ("stale_ready_nodes", "stale_ready_nodes"),
        ("failure_prone_nodes", "failure_prone_nodes"),
        ("collisions", "collisions"),
        ("project_cwd_mismatch", "project_cwd_mismatch"),
    ] {
        let f = count_or_len(first, key);
        let l = count_or_len(latest, key);
        let delta = l - f;
        let pct = if f == 0 {
            Value::Null
        } else {
            json!(((delta as f64 / f as f64) * 100.0 * 10.0).round() / 10.0)
        };
        summary.insert(
            label.to_string(),
            json!({ "first": f, "latest": l, "delta": delta, "percent_change": pct }),
        );
    }
    Value::Object(summary)
}

// ---------------------------------------------------------------------------
// The health report assembly and the health/trend CLI actions.
// ---------------------------------------------------------------------------

/// The full health report (triage.py cmd_health): twelve advisory sections
/// over the project-scoped entries plus the scoped totals, every externals
/// failure degrading to an absent section, never a broken run.
pub(crate) fn health_report(
    project: Option<&str>,
    all_projects: bool,
    stale_days: i64,
) -> Result<Value, String> {
    let all_entries = super::triage::triage_entries()?;
    let entries = super::triage::filter_by_project(&all_entries, project, all_projects);
    let pending: Vec<&Value> = entries
        .iter()
        .filter(|e| super::triage::is_pending(e) || super::triage::is_idea(e))
        .collect();
    let pending_active: Vec<&Value> = entries
        .iter()
        .filter(|e| super::triage::is_pending(e))
        .collect();
    let idea_count = pending.iter().filter(|e| super::triage::is_idea(e)).count() as i64;
    let mut stale = Vec::new();
    for e in &pending_active {
        if e.get("status").and_then(Value::as_str) != Some("ready") {
            continue;
        }
        let Some(created) = e.get("created_at").and_then(Value::as_str) else {
            continue;
        };
        let Some(created_ms) = crate::event_store::parse_rfc3339_ms(created) else {
            eprintln!(
                "Warning: cannot parse created_at on {}: {:?}",
                e["id"], created
            );
            continue;
        };
        let age_days = (super::advance::now_ms_i64() - created_ms) / 86_400_000;
        if age_days > stale_days {
            stale.push(json!({ "id": e["id"], "title": e["title"], "age_days": age_days }));
        }
    }
    let fp_min = {
        let raw = hm_config()["thresholds"]["failure_prone_attempts"]
            .as_i64()
            .unwrap_or(2);
        if raw < 1 {
            2
        } else {
            raw
        }
    };
    let mut failure_prone = Vec::new();
    for e in &pending_active {
        let sessions = e.get("cost_sessions").and_then(Value::as_array);
        let Some(sessions) = sessions else { continue };
        if (sessions.len() as i64) < fp_min || e.get("pr_number").and_then(Value::as_i64).is_some()
        {
            continue;
        }
        let burned: f64 = sessions
            .iter()
            .filter_map(|s| s.get("cost_usd").and_then(Value::as_f64))
            .sum();
        failure_prone.push(json!({
            "id": e["id"],
            "title": e["title"],
            "attempts": sessions.len(),
            "burned_usd": (burned * 100.0).round() / 100.0,
        }));
    }
    let repo_root = super::triage::intake_repo_root().unwrap_or_default();
    let mut collisions = Vec::new();
    let mut seen_pairs: std::collections::BTreeSet<(String, String)> = Default::default();
    for e in &pending_active {
        let Some(plan_path) = e.get("plan_path").and_then(Value::as_str) else {
            continue;
        };
        if plan_path.is_empty() {
            continue;
        }
        let resolved = crate::backlog::collision::resolve_plan_path(plan_path, &repo_root);
        let Some(node_id) = e.get("id").and_then(Value::as_str) else {
            continue;
        };
        let node_collisions = crate::backlog::collision::find_collisions(
            &resolved,
            &entries,
            node_id,
            &crate::backlog::collision::Thresholds::default(),
        );
        for c in node_collisions {
            let pair_id = c.with_node_id.clone();
            let mut pair = vec![node_id.to_string(), pair_id];
            pair.sort();
            let key = (pair[0].clone(), pair[1].clone());
            if seen_pairs.contains(&key) {
                continue;
            }
            seen_pairs.insert(key.clone());
            let severity_str = match c.severity {
                crate::backlog::collision::Severity::High => "high",
                crate::backlog::collision::Severity::Medium => "medium",
                crate::backlog::collision::Severity::Low => "low",
            };
            let action_str = match c.recommended_action {
                crate::backlog::collision::Action::Coordinate => "coordinate",
                crate::backlog::collision::Action::Absorb => "absorb",
                crate::backlog::collision::Action::Supersede => "supersede",
            };
            if matches!(
                c.severity,
                crate::backlog::collision::Severity::Medium
                    | crate::backlog::collision::Severity::High
            ) {
                collisions.push(json!({
                    "between": pair,
                    "shared_files": c.shared_files,
                    "severity": severity_str,
                    "recommended_action": action_str,
                }));
            }
        }
    }
    let ack = crate::backlog::collision::find_acknowledged_collisions(&entries);
    let resolved_payload: Vec<Value> = ack
        .iter()
        .map(|r| {
            json!({
                "node_id": r.node_id,
                "node_title": r.node_title,
                "resolved_via": r.resolved_via,
                "resolved_via_title": r.resolved_via_title,
                "resolved_via_status": r.resolved_via_status,
            })
        })
        .collect();
    let mut mismatch_ids: Vec<String> = Vec::new();
    for e in &pending_active {
        let Some(proj) = e.get("project").and_then(Value::as_str) else {
            continue;
        };
        let Some(root) = super::triage::project_root_from_settings(Some(proj)) else {
            continue;
        };
        let raw_cwd = e.get("cwd").and_then(Value::as_str).unwrap_or("");
        let normalized = if raw_cwd.is_empty() {
            String::new()
        } else {
            super::super::territory::normalize_path(raw_cwd)
        };
        if normalized != root {
            mismatch_ids.push(
                e.get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            );
        }
    }
    let stranded = stranded_dependents(&entries);
    let by_id: BTreeMap<String, &Value> = entries
        .iter()
        .filter_map(|e| {
            e.get("id")
                .and_then(Value::as_str)
                .map(|id| (id.to_string(), e))
        })
        .collect();
    let stranded_payload: Vec<Value> = stranded
        .iter()
        .map(|(blocker_id, dep_ids)| {
            let blocker = by_id.get(blocker_id).copied();
            json!({
                "blocker": blocker_id,
                "blocker_title": blocker.map(|b| b["title"].clone()).unwrap_or(Value::Null),
                "deferred_reason": blocker.map(|b| b["deferred_reason"].clone()).unwrap_or(Value::Null),
                "dependents": dep_ids.iter().map(|d| {
                    let dep = by_id.get(d).copied();
                    json!({
                        "id": d,
                        "title": dep.map(|x| x["title"].clone()).unwrap_or(Value::Null),
                        "status": dep.map(|x| x["status"].clone()).unwrap_or(Value::Null),
                    })
                }).collect::<Vec<_>>(),
            })
        })
        .collect();
    let mut ownership_defects = Vec::new();
    for e in &entries {
        let Some(defect) = e.get("ownership_defect").filter(|d| d.is_object()) else {
            continue;
        };
        ownership_defects.push(json!({
            "id": e.get("id"),
            "kind": defect.get("kind"),
            "holder": defect.get("holder"),
            "status": e.get("status"),
        }));
    }
    let batch = batch_verdict_for(project);
    let evals = evals_summary();
    let events = read_canonical_events();
    let routing_metrics = super::triage::fold_routing_health(&events);
    let triage_metrics = super::triage::fold_triage_health(&events);
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(root) = project.and_then(|p| super::triage::project_root_from_settings(Some(p))) {
        roots.push(PathBuf::from(root));
    }
    let dnm = done_not_merged_report(&entries, &roots);
    let index: BTreeMap<String, &Value> = all_entries
        .iter()
        .filter_map(|e| {
            e.get("id")
                .and_then(Value::as_str)
                .map(|id| (id.to_string(), e))
        })
        .collect();
    let mut supersession_unverified = Vec::new();
    for e in &entries {
        let Some(record) = e.get("supersession").filter(|r| r.is_object()) else {
            continue;
        };
        if record
            .get("verified_at")
            .map(|v| !v.is_null())
            .unwrap_or(false)
            || e.get("completed_at").map(|v| !v.is_null()).unwrap_or(false)
        {
            continue;
        }
        let Some(succ_id) = e.get("superseded_by").and_then(Value::as_str) else {
            continue;
        };
        let Some(successor) = index.get(succ_id) else {
            continue;
        };
        let Some(succ_done) = successor.get("completed_at").and_then(Value::as_str) else {
            continue;
        };
        if succ_done.is_empty() || successor.get("pr_number").and_then(Value::as_i64).is_none() {
            continue;
        }
        let surfaces = record
            .get("surfaces")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let matched = record
            .get("matched_surfaces")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let uncovered: Vec<Value> = surfaces
            .iter()
            .filter(|s| !matched.contains(s))
            .cloned()
            .collect();
        supersession_unverified.push(json!({
            "predecessor": e.get("id"),
            "successor": succ_id,
            "evidence_pr": successor.get("pr_number"),
            "uncovered_surfaces": uncovered,
        }));
    }
    let mut blocked_by_held = Vec::new();
    for e in &entries {
        let raw = e
            .get("blocked_reason")
            .and_then(Value::as_str)
            .unwrap_or("");
        let (kind, rest) = raw.split_once(':').unwrap_or(("", ""));
        if !matches!(kind, "blocked-by-deferred" | "unknown-dep") || rest.is_empty() {
            continue;
        }
        let blocker = index.get(rest).copied();
        let blocker_status = match blocker {
            Some(b) => {
                let st = b.get("status").and_then(Value::as_str).unwrap_or("");
                if st.is_empty() {
                    if b.get("completed_at").map(|v| !v.is_null()).unwrap_or(false) {
                        "done"
                    } else {
                        "open"
                    }
                } else {
                    st
                }
            }
            None => "missing",
        };
        blocked_by_held.push(json!({
            "node": e.get("id"),
            "blocker": rest,
            "blocker_status": blocker_status,
        }));
    }
    let (orphan_rate, orphan_nodes) = orphan_fold(&entries, &all_entries);
    let mut report = Map::new();
    report.insert(
        "scope".into(),
        json!(super::triage::resolve_scope(
            project,
            all_projects,
            &all_entries
        )),
    );
    if let Some(r) = routing_metrics {
        report.insert("routing".into(), r);
    }
    if let Some(t) = triage_metrics {
        report.insert("triage_metrics".into(), t);
    }
    report.insert("idea_pile_depth".into(), json!(idea_count));
    let stale_count = stale.len();
    let failure_prone_count = failure_prone.len();
    report.insert("stale_ready_nodes".into(), Value::Array(stale));
    report.insert("failure_prone_nodes".into(), Value::Array(failure_prone));
    report.insert("collisions".into(), Value::Array(collisions.clone()));
    report.insert(
        "acknowledged_resolved".into(),
        Value::Array(resolved_payload.clone()),
    );
    report.insert(
        "project_cwd_mismatch".into(),
        json!(mismatch_ids.len() as i64),
    );
    report.insert("project_cwd_mismatch_nodes".into(), json!(mismatch_ids));
    if let Some(rate) = orphan_rate {
        report.insert("orphan_feature_rate".into(), json!(rate));
        report.insert("orphan_feature_nodes".into(), json!(orphan_nodes));
    }
    report.insert("done_not_merged".into(), dnm["violations"].clone());
    report.insert("done_not_merged_unknown".into(), dnm["unknown"].clone());
    report.insert(
        "supersession_unverified".into(),
        Value::Array(supersession_unverified),
    );
    report.insert("blocked_by_held".into(), Value::Array(blocked_by_held));
    report.insert(
        "stranded_by_failed_blocker".into(),
        Value::Array(stranded_payload.clone()),
    );
    report.insert(
        "ownership_defects".into(),
        Value::Array(ownership_defects.clone()),
    );
    if let Some(v) = batch {
        report.insert("batch_verdict".into(), json!(v));
    }
    if let Some(v) = evals {
        report.insert("evals".into(), v);
    }
    report.insert("totals".into(), json!({
        "pending": pending_active.len(),
        "ideas": idea_count,
        "stale": stale_count,
        "failure_prone": failure_prone_count,
        "collisions": collisions.len(),
        "acknowledged_resolved": resolved_payload.len(),
        "project_cwd_mismatch": mismatch_ids.len(),
        "stranded_by_failed_blocker": stranded_payload.iter().map(|s| s["dependents"].as_array().map(Vec::len).unwrap_or(0)).sum::<usize>(),
        "ownership_defects": ownership_defects.len(),
    }));
    Ok(Value::Object(report))
}

/// The batch events journal the verdict reads: a scoped project reads ITS
/// root's journal; unmapped scopes read the canonical root's.
fn batch_verdict_for(project: Option<&str>) -> Option<String> {
    let root: PathBuf =
        match project.and_then(|p| super::triage::project_root_from_settings(Some(p))) {
            Some(r) => PathBuf::from(r),
            None => {
                let cwd = std::env::current_dir().ok()?;
                crate::paths::canonical_repo_root(&cwd).unwrap_or(cwd)
            }
        };
    batch_verdict(&root.join(".fno").join("events.jsonl"))
}

// ---------------------------------------------------------------------------
// The CLI actions: report + thresholds + history + render.
// ---------------------------------------------------------------------------

/// `triage health`: the aggregate report, optional threshold evaluation,
/// history append, notifications, and the text/JSON renders.
pub fn run_health(args: &[String]) -> i32 {
    let mut project: Option<String> = None;
    let mut all_projects = false;
    let mut json_output = false;
    let mut stale_days: i64 = 30;
    let mut check = false;
    let mut quiet = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--project" => project = it.next().cloned(),
            "--all" | "-A" => all_projects = true,
            "--json" | "-J" => json_output = true,
            "--stale-days" => stale_days = it.next().and_then(|v| v.parse().ok()).unwrap_or(30),
            "--check" => check = true,
            "--quiet" => quiet = true,
            _ => {}
        }
    }
    let report = match health_report(project.as_deref(), all_projects, stale_days) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("fno triage: {e}");
            return 2;
        }
    };
    let config = hm_config();
    if quiet && !check {
        eprintln!(
            "Note: --quiet has no effect without --check (this command is always silent in healthy state when --check is set)."
        );
    }
    if check {
        let breaches = evaluate_thresholds(&report, &config);
        let history_enabled = config["history"]["enabled"].as_bool().unwrap_or(true);
        if history_enabled {
            let mut retain = config["history"]["retain_days"].as_i64().unwrap_or(90);
            if retain < 0 {
                retain = 90;
            }
            if let Some(path) = history_path(&config) {
                append_history(&report, &breaches, &path, retain);
            }
        }
        if !breaches.is_empty() {
            dispatch_notifications(&report, &breaches, &config);
            if !quiet {
                if json_output {
                    echo_json(
                        &json!({ "status": "breach", "report": report, "breaches": breaches }),
                    );
                } else {
                    println!("Backlog health: BREACH");
                    for b in &breaches {
                        println!(
                            "  [{}] {}: actual={}, threshold={}",
                            b["severity"].as_str().unwrap_or("").to_uppercase(),
                            b["key"].as_str().unwrap_or(""),
                            b["actual"],
                            b["threshold"]
                        );
                    }
                }
            }
            return 4;
        }
        if !quiet {
            if json_output {
                echo_json(&json!({ "status": "healthy", "report": report }));
            } else {
                println!(
                    "Backlog health: OK (no thresholds breached) - {}",
                    report["scope"].as_str().unwrap_or("")
                );
            }
        }
        return 0;
    }
    if json_output {
        echo_json(&report);
        return 0;
    }
    println!("Backlog health: {}", report["scope"].as_str().unwrap_or(""));
    println!("  pending: {}", report["totals"]["pending"]);
    println!("  ideas: {}", report["idea_pile_depth"]);
    println!(
        "  stale (>{stale_days}d ready): {}",
        report["totals"]["stale"]
    );
    println!(
        "  failure-prone (>1 attempt, no PR): {}",
        report["totals"]["failure_prone"]
    );
    println!("  collisions (medium+): {}", report["totals"]["collisions"]);
    println!(
        "  acknowledged-resolved: {}",
        report["totals"]["acknowledged_resolved"]
    );
    println!(
        "  project<->cwd mismatches: {}",
        report["totals"]["project_cwd_mismatch"]
    );
    if let Some(rate) = report.get("orphan_feature_rate").and_then(Value::as_f64) {
        let n = report["orphan_feature_nodes"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0);
        println!(
            "  orphan features (no mission edge): {:.0}% ({n})",
            rate * 100.0
        );
    }
    println!(
        "  stranded by failed blocker: {}",
        report["totals"]["stranded_by_failed_blocker"]
    );
    if let Some(evals) = report.get("evals").filter(|e| e.is_object()) {
        let rate = evals["regression_pass_rate"].as_f64();
        let alarm = if evals["regression_alarm"]
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false)
        {
            " ALARM"
        } else {
            ""
        };
        let regressed_n = evals["regressed"].as_array().map(Vec::len).unwrap_or(0);
        let regressed_txt = if regressed_n > 0 {
            format!(" REGRESSED {regressed_n}")
        } else {
            String::new()
        };
        let mut rate_txt = String::new();
        if let Some(r) = rate {
            rate_txt = format!("regression pass {:.0}%, ", r * 100.0);
        }
        let age = evals["age_days"].as_i64();
        let age_txt = if evals["never_ran"].as_bool().unwrap_or(false) {
            " age never".to_string()
        } else {
            age.map(|a| format!(" age {a}d")).unwrap_or_default()
        };
        let stale_txt = if evals["stale"].as_bool().unwrap_or(false) {
            " STALE"
        } else {
            ""
        };
        println!(
            "  evals: {rate_txt}flakes {}{age_txt}{stale_txt}{alarm}{regressed_txt}",
            evals["flake_count"]
        );
    }
    if let Some(rm) = report.get("routing").filter(|e| e.is_object()) {
        let total = rm["total"].as_i64().unwrap_or(0);
        println!();
        println!("Executor routing ({total} resolutions):");
        let mut dist_parts = Vec::new();
        if let Some(tiers) = rm["tier_distribution"].as_object() {
            let mut keys: Vec<&String> = tiers.keys().collect();
            keys.sort();
            for t in keys {
                dist_parts.push(format!("{t}={}/{}", tiers[t], total));
            }
        }
        println!("  tier distribution: {}", dist_parts.join(", "));
        println!("  inference share: {}/{}", rm["inference"], total);
        println!("  warn-fallback: {}/{}", rm["warn_fallback_count"], total);
        let inferred = rm["inferred_tasks"].as_i64().unwrap_or(0);
        if inferred > 0 {
            println!(
                "  override-after-inference (mis-route proxy): {}/{}",
                rm["overridden_after_inference"], inferred
            );
        } else {
            println!("  override-after-inference (mis-route proxy): n/a (no inference-resolved tasks with ids)");
        }
    }
    if let Some(tm) = report.get("triage_metrics").filter(|e| e.is_object()) {
        println!();
        println!("Triage applies ({}):", tm["applies"]);
        println!(
            "  applied: priority={}, deps={}, dups={}, deferred={}",
            tm["applied_by_category"]["priority_changes"],
            tm["applied_by_category"]["dependencies"],
            tm["applied_by_category"]["duplicates_flagged"],
            tm["applied_by_category"]["deferred"]
        );
        let proposed = tm["proposed"].as_i64().unwrap_or(0);
        if proposed > 0 {
            println!("  validation drop rate: {}/{}", tm["dropped"], proposed);
        } else {
            println!("  validation drop rate: n/a (no proposal entries recorded)");
        }
    }
    match report.get("batch_verdict").and_then(Value::as_str) {
        Some("build-wave4") => println!(
            "  batch-lane verdict: build-wave4 - abandonment waste exceeds savings; consider building batch-lane Wave 4 (surgical isolation). See `fno backlog batch metrics`."
        ),
        Some("disable-batching") => println!(
            "  batch-lane verdict: disable-batching - batching costs more CI than it saves; consider config.batch.enabled: false. See `fno backlog batch metrics`."
        ),
        _ => {}
    }
    if let Some(rows) = report["collisions"].as_array().filter(|r| !r.is_empty()) {
        println!();
        println!("Plans stepping on each other:");
        for col in rows {
            let between = col["between"]
                .as_array()
                .map(|a| a.iter().map(value_brief).collect::<Vec<_>>().join(" <-> "))
                .unwrap_or_default();
            println!(
                "  [{}] {}: {} shared files; recommend {}",
                col["severity"].as_str().unwrap_or(""),
                between,
                col["shared_files"].as_array().map(Vec::len).unwrap_or(0),
                col["recommended_action"].as_str().unwrap_or("")
            );
        }
    }
    if let Some(rows) = report["acknowledged_resolved"]
        .as_array()
        .filter(|r| !r.is_empty())
    {
        println!();
        println!("Acknowledged collisions now resolved (verify cleanup):");
        for r in rows {
            println!(
                "  {} acknowledged collision with {} ({}); review whether the conflict resolved cleanly.",
                r["node_id"].as_str().unwrap_or(""),
                r["resolved_via"].as_str().unwrap_or(""),
                r["resolved_via_status"].as_str().unwrap_or("")
            );
        }
    }
    if let Some(nodes) = report["project_cwd_mismatch_nodes"]
        .as_array()
        .filter(|r| !r.is_empty())
    {
        println!();
        println!("Pending nodes with project<->cwd mismatch (producer regression):");
        for node_id in nodes {
            println!("  {}", node_id.as_str().unwrap_or(""));
        }
    }
    if let Some(rows) = report["stranded_by_failed_blocker"]
        .as_array()
        .filter(|r| !r.is_empty())
    {
        println!();
        println!("Stranded by a failed blocker (recover via undefer or fix the blocker):");
        for s in rows {
            println!(
                "  {} deferred ({}) strands:",
                s["blocker"].as_str().unwrap_or(""),
                s["deferred_reason"].as_str().unwrap_or("")
            );
            for d in s["dependents"].as_array().unwrap_or(&vec![]) {
                println!(
                    "    - {} [{}] {}",
                    d["id"].as_str().unwrap_or(""),
                    d["status"].as_str().unwrap_or(""),
                    d["title"].as_str().unwrap_or("")
                );
            }
        }
    }
    if let Some(rows) = report["ownership_defects"]
        .as_array()
        .filter(|r| !r.is_empty())
    {
        println!();
        println!("Ownership defects (age records uncertainty, never an owner-death verdict):");
        for d in rows {
            println!(
                "  {} [{}] {} holder={}",
                d["id"].as_str().unwrap_or(""),
                d["status"].as_str().unwrap_or(""),
                d["kind"].as_str().unwrap_or(""),
                d["holder"].as_str().unwrap_or("")
            );
        }
        println!(
            "  liveness is not decided here. For each row: the transcript mtime for the holder session under the harness projects dir, AND the registry row from `fno agents list --json`. A claim lockfile confirms a live holder; its absence confirms nothing."
        );
    }
    let dnm_violations = report["done_not_merged"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if !dnm_violations.is_empty() {
        println!();
        println!("Closed over an unmerged PR (last 7d) - the map claims work that has not landed:");
        for v in &dnm_violations {
            println!(
                "  {} PR #{} is {}, closed {}  {}",
                v["id"].as_str().unwrap_or(""),
                v["pr_number"],
                v["pr_state"].as_str().unwrap_or(""),
                v["completed_at"].as_str().unwrap_or(""),
                v["title"].as_str().unwrap_or("")
            );
        }
    }
    let dnm_unknown = report["done_not_merged_unknown"]
        .as_array()
        .map(Vec::len)
        .unwrap_or(0);
    if dnm_unknown > 0 {
        println!();
        println!("done=merged unknown (not a violation): {dnm_unknown} node(s)");
    }
    0
}

/// `triage trend`: the rolling-window readout over the history file.
pub fn run_trend(args: &[String]) -> i32 {
    let mut days: i64 = 7;
    let mut json_output = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--days" => days = it.next().and_then(|v| v.parse().ok()).unwrap_or(7),
            "--json" | "-J" => json_output = true,
            _ => {}
        }
    }
    let config = hm_config();
    let Some(path) = history_path(&config) else {
        eprintln!("fno triage: no history path");
        return 2;
    };
    let entries = read_history(&path, days);
    let summary = summarize_trend(&entries);
    if json_output {
        echo_json(&json!({ "days": days, "entries": entries.len(), "summary": summary }));
        return 0;
    }
    if entries.is_empty() {
        println!("Backlog trend (last {days} days): no history yet.");
        return 0;
    }
    println!(
        "Backlog trend (last {days} days, {} entries):",
        entries.len()
    );
    if let Some(cats) = summary.as_object() {
        for (key, stats) in cats {
            let delta = stats["delta"].as_i64().unwrap_or(0);
            let sign = if delta >= 0 { "+" } else { "" };
            let pct = match stats["percent_change"].as_f64() {
                Some(p) => format!("{sign}{p}%"),
                None => "n/a".to_string(),
            };
            println!(
                "  {key}: {} -> {} ({sign}{delta}, {pct})",
                stats["first"], stats["latest"]
            );
        }
    }
    0
}

/// One-line digest of a JSON value for a render or error string.
fn value_brief(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}
