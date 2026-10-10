//! Native triage engine: the deterministic folds, ranking, and proposal
//! validation of `fno/graph/triage.py`, ported as pure functions over JSON
//! so the characterization goldens feed them parsed fixtures directly.
//! The LLM-reasoning legs (propose, consistency runs) stay in the CLI
//! module; the graph mutation (apply) rides the store's locked write.

use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};

/// The cwd the project auto-detection reads. A session running from a
/// subdirectory still resolves through the git toplevel the way the Python
/// intake's repo_root does.
pub(crate) fn intake_repo_root() -> Option<std::path::PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    Some(crate::paths::worktree_repo_root(&cwd))
}

/// Project auto-detection from node cwd fields (graph/_intake.py
/// detect_project): the first node whose cwd normalizes to the repo root
/// names the project; else the first node under the repo root does.
pub fn detect_project(entries: &[Value]) -> Option<String> {
    let root = intake_repo_root()?;
    let norm_root = super::super::territory::normalize_path(&root.to_string_lossy());
    let root_with_sep = format!("{}/", norm_root.trim_end_matches('/'));
    let mut fallback: Option<String> = None;
    for e in entries {
        let Some(raw) = e.get("cwd").and_then(Value::as_str) else {
            continue;
        };
        if raw.is_empty() {
            continue;
        }
        let norm_cwd = super::super::territory::normalize_path(raw);
        if norm_cwd == norm_root {
            return e.get("project").and_then(Value::as_str).map(str::to_string);
        }
        if fallback.is_none() && norm_cwd.starts_with(&root_with_sep) {
            fallback = e.get("project").and_then(Value::as_str).map(str::to_string);
        }
    }
    fallback
}

/// The work-map root for one project name, or None (graph/_intake.py
/// project_root_from_settings): a pure map lookup over the same workspace
/// shapes both sides read; unmapped or absent reads None, never an error.
pub fn project_root_from_settings(project: Option<&str>) -> Option<String> {
    let project = project.filter(|p| !p.is_empty())?;
    let cwd = std::env::current_dir().ok()?;
    let map = crate::territory::workspace_paths(&cwd);
    map.get(project).cloned()
}

/// Scope the entries the way the collectors do (graph/_intake.py
/// filter_by_project): an explicit project filters to it, `show_all`
/// passes through, else the auto-detected project filters, else all.
pub fn filter_by_project(entries: &[Value], project: Option<&str>, show_all: bool) -> Vec<Value> {
    if let Some(project) = project.filter(|p| !p.is_empty()) {
        return entries
            .iter()
            .filter(|e| e.get("project").and_then(Value::as_str) == Some(project))
            .cloned()
            .collect();
    }
    if show_all {
        return entries.to_vec();
    }
    match detect_project(entries) {
        Some(detected) => entries
            .iter()
            .filter(|e| e.get("project").and_then(Value::as_str) == Some(&detected))
            .cloned()
            .collect(),
        None => entries.to_vec(),
    }
}

/// The human scope line the report and context carry.
pub fn resolve_scope(project: Option<&str>, all_projects: bool, entries: &[Value]) -> String {
    if let Some(project) = project.filter(|p| !p.is_empty()) {
        return format!("project '{project}'");
    }
    if all_projects {
        return "all projects".to_string();
    }
    match detect_project(entries) {
        Some(detected) => format!("project '{detected}' (auto-detected)"),
        None => {
            "all projects (no project detected - run an intake to register this repo)".to_string()
        }
    }
}

/// The priority ladder's rank (graph/_constants.py PRIORITY_ORDER).
pub fn priority_order(priority: Option<&str>) -> i64 {
    match priority {
        Some("p0") => 0,
        Some("p1") => 1,
        Some("p2") => 2,
        Some("p3") => 3,
        _ => 2,
    }
}

/// Ready or blocked, not done/deferred/claimed/idea/roadmap-row. Ideas are
/// intentionally excluded - the reasoning layer recommends writing a spec
/// for them rather than treating them as claimable work-in-progress - and
/// deferred rows are excluded so proposals never re-suggest a paused node.
pub fn is_pending(entry: &Value) -> bool {
    if entry.get("type").and_then(Value::as_str) == Some("roadmap") {
        return false;
    }
    let completed = entry
        .get("completed_at")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !completed.is_empty() {
        return false;
    }
    let status = entry
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("ready");
    if status == "deferred" {
        return false;
    }
    status == "ready" || status == "blocked"
}

/// Idea-stage row: plan-less, not claimed, not blocked, not done.
pub fn is_idea(entry: &Value) -> bool {
    if entry.get("type").and_then(Value::as_str) == Some("roadmap") {
        return false;
    }
    let completed = entry
        .get("completed_at")
        .map(|v| !v.is_null())
        .unwrap_or(false);
    if completed {
        return false;
    }
    entry.get("status").and_then(Value::as_str) == Some("idea")
}

/// First `max_lines` lines of the plan, or the empty string on any failure.
pub fn plan_excerpt(plan_path: Option<&str>, max_lines: usize) -> String {
    let Some(path) = plan_path.filter(|p| !p.is_empty()) else {
        return String::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return String::new();
    };
    text.lines()
        .take(max_lines)
        .fold(String::new(), |mut out, line| {
            out.push_str(line);
            out.push('\n');
            out
        })
}

/// Defensive candidate projection: legacy/corrupted graph entries may carry
/// a non-list cost_sessions or non-numeric costs, so the record filters to
/// well-formed sessions and aligns session_count with the same denominator
/// total_cost_usd uses.
pub fn candidate_record(entry: &Value, deep: bool) -> Value {
    let raw_sessions = entry.get("cost_sessions").cloned().unwrap_or(Value::Null);
    let cost_total: f64 = raw_sessions
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|s| {
                    let c = s.get("cost_usd")?;
                    match c {
                        Value::Number(n) => n.as_f64(),
                        _ => None,
                    }
                })
                .sum()
        })
        .unwrap_or(0.0);
    let session_count = raw_sessions
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter(|s| matches!(s.get("cost_usd"), Some(Value::Number(_))))
                .count() as i64
        })
        .unwrap_or(0);
    let mut record = json!({
        "id": entry.get("id"),
        "title": entry.get("title"),
        "priority": entry.get("priority").cloned().unwrap_or(Value::from("p2")),
        "blocked_by": entry.get("blocked_by").cloned().unwrap_or(Value::Array(vec![])),
        "plan_path": entry.get("plan_path"),
        "roadmap_id": entry.get("roadmap_id"),
        "created_at": entry.get("created_at"),
        "source": entry.get("source"),
        "status": entry.get("status"),
        "size": entry.get("size"),
        "domain": entry.get("domain"),
        "details": entry.get("details"),
        "claim_history": {
            "session_count": session_count,
            "total_cost_usd": (cost_total * 100.0).round() / 100.0,
            "last_locked_at": entry
                .get("locked_at")
                .or_else(|| entry.get("claimed_at"))
                .cloned()
                .unwrap_or(Value::Null),
        },
        "ship_state": {
            "pr_number": entry.get("pr_number"),
            "merge_status": entry.get("merge_status"),
        },
    });
    if deep {
        if let Some(obj) = record.as_object_mut() {
            if let Some(plan) = entry.get("plan_path").and_then(Value::as_str) {
                if !plan.is_empty() {
                    obj.insert(
                        "plan_excerpt".to_string(),
                        Value::from(plan_excerpt(Some(plan), 150)),
                    );
                }
            }
        }
    }
    record
}

/// Sort key fold shared by the pending and idea collectors: priority rank,
/// then created_at, ascending and stable.
pub fn sort_entries_by_priority_created(entries: &mut [Value]) {
    entries.sort_by(|a, b| {
        let pa = priority_order(a.get("priority").and_then(Value::as_str));
        let pb = priority_order(b.get("priority").and_then(Value::as_str));
        let ca = a.get("created_at").and_then(Value::as_str).unwrap_or("");
        let cb = b.get("created_at").and_then(Value::as_str).unwrap_or("");
        pa.cmp(&pb).then(ca.cmp(cb))
    });
}

/// One guarded graph read for triage's diagnostic surfaces: full records
/// under the default backend. An external-backend refusal is the caller's
/// named exit, never stale local rows.
pub fn triage_entries() -> Result<Vec<Value>, String> {
    let graph = super::settings::graph_path();
    crate::graph_store::read_rows(&graph).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Copeland ranking (comparative judgment folded into one stable order)
// ---------------------------------------------------------------------------

/// Order `ids` best-first from pairwise verdicts via Copeland score. Each
/// verdict is `{"winner": id, "loser": id}`; the score is wins minus
/// losses; contradictory or cyclic verdicts are tolerated (they net out)
/// rather than rejected, and a verdict naming an id outside `ids` is
/// ignored. Deterministic tiebreak: higher net, then `meta` (priority rank
/// asc, created_at asc), then id.
pub fn copeland_rank(
    ids: &[String],
    verdicts: &[Value],
    meta: Option<&BTreeMap<String, (i64, String)>>,
) -> Vec<Value> {
    let mut idset: Vec<String> = Vec::new();
    let mut present: BTreeSet<&str> = BTreeSet::new();
    for id in ids {
        if present.insert(id.as_str()) {
            idset.push(id.clone());
        }
    }
    let mut wins: BTreeMap<&str, i64> = idset.iter().map(|i| (i.as_str(), 0)).collect();
    let mut losses: BTreeMap<&str, i64> = idset.iter().map(|i| (i.as_str(), 0)).collect();
    for v in verdicts {
        let (Some(win), Some(lose)) = (
            v.get("winner").and_then(Value::as_str),
            v.get("loser").and_then(Value::as_str),
        ) else {
            continue;
        };
        if win != lose && present.contains(win) && present.contains(lose) {
            *wins.entry(win).or_insert(0) += 1;
            *losses.entry(lose).or_insert(0) += 1;
        }
    }
    let no_meta: BTreeMap<String, (i64, String)> = BTreeMap::new();
    let meta = meta.unwrap_or(&no_meta);
    let mut ranked: Vec<&String> = idset.iter().collect();
    ranked.sort_by(|a, b| {
        let net_a = wins[a.as_str()] - losses[a.as_str()];
        let net_b = wins[b.as_str()] - losses[b.as_str()];
        let (prio_a, created_a) = meta.get(a.as_str()).cloned().unwrap_or((99, String::new()));
        let (prio_b, created_b) = meta.get(b.as_str()).cloned().unwrap_or((99, String::new()));
        net_b
            .cmp(&net_a)
            .then(prio_a.cmp(&prio_b))
            .then(created_a.cmp(&created_b))
            .then(a.cmp(b))
    });
    ranked
        .into_iter()
        .map(|i| {
            let w = wins[i.as_str()];
            let l = losses[i.as_str()];
            json!({"id": i, "wins": w, "losses": l, "net": w - l})
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Proposal validation: drops cycles and unknown-id entries
// ---------------------------------------------------------------------------

/// `id -> set(blocked_by)` over the graph entries.
pub fn build_dependency_map(entries: &[Value]) -> BTreeMap<String, BTreeSet<String>> {
    let mut result: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for e in entries {
        let Some(eid) = e.get("id").and_then(Value::as_str) else {
            continue;
        };
        let blockers: BTreeSet<String> = e
            .get("blocked_by")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        result.insert(eid.to_string(), blockers);
    }
    result
}

/// True if adding edge `to blocked_by frm` creates a cycle.
pub fn would_cycle(graph: &BTreeMap<String, BTreeSet<String>>, frm: &str, to: &str) -> bool {
    let mut stack = vec![frm.to_string()];
    let mut seen: BTreeSet<String> = BTreeSet::new();
    while let Some(node) = stack.pop() {
        if node == to {
            return true;
        }
        if !seen.insert(node.clone()) {
            continue;
        }
        if let Some(blockers) = graph.get(&node) {
            for blocker in blockers {
                stack.push(blocker.clone());
            }
        }
    }
    false
}

/// Return (cleaned, errors). Drops cycles and unknown-id entries; a defer
/// entry without a reason is dropped so a paused node always carries the
/// rationale into the kanban view.
pub fn validate_proposal(proposal: &Value, entries: &[Value]) -> (Value, Vec<String>) {
    let mut errors: Vec<String> = Vec::new();
    let valid_ids: BTreeSet<&str> = entries
        .iter()
        .filter_map(|e| e.get("id").and_then(Value::as_str))
        .collect();

    let empty: Vec<Value> = Vec::new();
    let deps_in = proposal
        .get("dependencies")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let mut dep_map = build_dependency_map(entries);
    let mut clean_deps: Vec<Value> = Vec::new();
    for edge in deps_in {
        let Some(obj) = edge.as_object() else {
            errors.push(format!("dependency entry is not an object: {edge}"));
            continue;
        };
        let (Some(frm), Some(to)) = (
            obj.get("from").and_then(Value::as_str),
            obj.get("to").and_then(Value::as_str),
        ) else {
            errors.push(format!(
                "dependency references unknown id(s): {:?} -> {:?}",
                obj.get("from"),
                obj.get("to")
            ));
            continue;
        };
        if !valid_ids.contains(frm) || !valid_ids.contains(to) {
            errors.push(format!(
                "dependency references unknown id(s): {frm} -> {to}"
            ));
            continue;
        }
        if frm == to {
            errors.push(format!("self-dependency ignored: {frm}"));
            continue;
        }
        if would_cycle(&dep_map, frm, to) {
            errors.push(format!(
                "cycle: adding {to} blocked_by {frm} would cycle - dropping edge"
            ));
            continue;
        }
        dep_map
            .entry(to.to_string())
            .or_default()
            .insert(frm.to_string());
        clean_deps.push(edge.clone());
    }

    let prio_in = proposal
        .get("priority_changes")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let mut clean_prio: Vec<Value> = Vec::new();
    for pc in prio_in {
        let Some(obj) = pc.as_object() else {
            errors.push(format!("priority_change entry is not an object: {pc}"));
            continue;
        };
        let pid = obj.get("id");
        let to_p = obj.get("to").and_then(Value::as_str);
        let id_ok = pid
            .and_then(Value::as_str)
            .map(|s| valid_ids.contains(s))
            .unwrap_or(false);
        if !id_ok {
            errors.push(format!("priority_change references unknown id: {pid:?}"));
            continue;
        }
        let to_ok = matches!(to_p, Some("p0") | Some("p1") | Some("p2") | Some("p3"));
        if !to_ok {
            errors.push(format!(
                "priority_change invalid priority: {:?}",
                obj.get("to")
            ));
            continue;
        }
        clean_prio.push(pc.clone());
    }

    let dups_in = proposal
        .get("duplicates")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let mut clean_dups: Vec<Value> = Vec::new();
    for dup in dups_in {
        let Some(obj) = dup.as_object() else {
            errors.push(format!("duplicate entry is not an object: {dup}"));
            continue;
        };
        let Some(ids) = obj.get("ids").and_then(Value::as_array) else {
            errors.push(format!(
                "duplicate entry requires ids list with >=2 elements: {dup}"
            ));
            continue;
        };
        if ids.len() < 2 {
            errors.push(format!(
                "duplicate entry requires ids list with >=2 elements: {dup}"
            ));
            continue;
        }
        let bad: Vec<&str> = ids
            .iter()
            .filter_map(Value::as_str)
            .filter(|i| !valid_ids.contains(i))
            .collect();
        if !bad.is_empty() {
            errors.push(format!("duplicate references unknown id(s): {bad:?}"));
            continue;
        }
        clean_dups.push(dup.clone());
    }

    let defer_in = proposal
        .get("defer")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let mut clean_defer: Vec<Value> = Vec::new();
    for d in defer_in {
        let Some(obj) = d.as_object() else {
            errors.push(format!("defer entry is not an object: {d}"));
            continue;
        };
        let did = obj.get("id");
        let reason = obj
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let id_ok = did
            .and_then(Value::as_str)
            .map(|s| valid_ids.contains(s))
            .unwrap_or(false);
        if !id_ok {
            errors.push(format!("defer references unknown id: {did:?}"));
            continue;
        }
        if reason.is_empty() {
            errors.push(format!(
                "defer entry missing reason: {}",
                did.cloned().unwrap_or(Value::Null)
            ));
            continue;
        }
        clean_defer.push(json!({"id": did.cloned().unwrap_or(Value::Null), "reason": reason}));
    }

    (
        json!({
            "dependencies": clean_deps,
            "priority_changes": clean_prio,
            "duplicates": clean_dups,
            "defer": clean_defer,
        }),
        errors,
    )
}

// ---------------------------------------------------------------------------
// Health folds (event-gated; no fabricated zeros)
// ---------------------------------------------------------------------------

fn event_data(e: &Value) -> Value {
    e.get("data")
        .cloned()
        .or_else(|| e.get("payload").cloned())
        .unwrap_or(Value::Null)
}

/// Fold `executor_resolved` events into routing-tier metrics. Returns None
/// when no such events exist so the health render can gate the section. The
/// override-after-inference count keys by (plan_path, task): task ids like
/// "1.1" are plan-relative, not global.
pub fn fold_routing_health(events: &[Value]) -> Option<Value> {
    let er: Vec<&Value> = events
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str) == Some("executor_resolved"))
        .collect();
    if er.is_empty() {
        return None;
    }
    let mut tiers: BTreeMap<String, i64> = BTreeMap::new();
    let mut warn = 0i64;
    let mut inferred: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut overridden: BTreeSet<(String, String)> = BTreeSet::new();
    for e in &er {
        let d = event_data(e);
        let tier = d
            .get("tier")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string();
        *tiers.entry(tier.clone()).or_insert(0) += 1;
        if d.get("warn_fallback")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            warn += 1;
        }
        let task = d.get("task").and_then(Value::as_str).unwrap_or("");
        if task.is_empty() {
            continue;
        }
        let key = (
            d.get("plan_path")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            task.to_string(),
        );
        let mut resolved = d
            .get("resolved")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if resolved == "do" {
            resolved = "tdd".to_string();
        }
        if tier == "surface-inference" {
            inferred.entry(key).or_insert(resolved);
        } else if tier == "task-block" || tier == "plan-frontmatter" {
            if let Some(prev) = inferred.get(&key) {
                if *prev != resolved {
                    overridden.insert(key);
                }
            }
        }
    }
    Some(json!({
        "total": er.len(),
        "tier_distribution": tiers,
        "inference": tiers.get("surface-inference").copied().unwrap_or(0),
        "warn_fallback_count": warn,
        "inferred_tasks": inferred.len(),
        "overridden_after_inference": overridden.len(),
    }))
}

/// Fold `triage_applied` events into apply-count plus validation-drop
/// metrics. Returns None when absent. The drop rate ships both numerator
/// and denominator so it is never a bare percentage.
pub fn fold_triage_health(events: &[Value]) -> Option<Value> {
    let ta: Vec<&Value> = events
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str) == Some("triage_applied"))
        .collect();
    if ta.is_empty() {
        return None;
    }
    let mut cats: BTreeMap<String, i64> = BTreeMap::new();
    for key in [
        "priority_changes",
        "dependencies",
        "duplicates_flagged",
        "deferred",
    ] {
        cats.insert(key.to_string(), 0);
    }
    let mut proposed = 0i64;
    let mut dropped = 0i64;
    for e in &ta {
        let d = event_data(e);
        let a = d.get("applied").cloned().unwrap_or(Value::Null);
        if let Some(obj) = a.as_object() {
            let keys: Vec<String> = cats.keys().cloned().collect();
            for key in keys {
                let v = obj.get(&key).cloned().unwrap_or(Value::Null);
                let n = match v {
                    Value::Number(n) => n.as_i64().unwrap_or(0),
                    Value::String(s) => s.trim().parse().unwrap_or(0),
                    _ => 0,
                };
                *cats.entry(key).or_insert(0) += n;
            }
        }
        for (field, total) in [("proposed", &mut proposed), ("dropped", &mut dropped)] {
            let v = d.get(field).cloned().unwrap_or(Value::Null);
            let n = match v {
                Value::Number(n) => n.as_i64().unwrap_or(0),
                Value::String(s) => s.trim().parse().unwrap_or(0),
                _ => 0,
            };
            *total += n;
        }
    }
    Some(json!({
        "applies": ta.len(),
        "applied_by_category": cats,
        "proposed": proposed,
        "dropped": dropped,
    }))
}

// ---------------------------------------------------------------------------
// Consistency folds (agreement across completed propose runs)
// ---------------------------------------------------------------------------

/// {node id -> proposed `to`} for a proposal's priority_changes.
fn priority_map(proposal: &Value) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    if let Some(rows) = proposal.get("priority_changes").and_then(Value::as_array) {
        for pc in rows {
            if let Some(obj) = pc.as_object() {
                if let Some(id) = obj.get("id").and_then(Value::as_str) {
                    if !id.is_empty() {
                        out.insert(
                            id.to_string(),
                            obj.get("to").cloned().unwrap_or(Value::Null),
                        );
                    }
                }
            }
        }
    }
    out
}

/// {key -> true} for a category whose agreement is presence-based.
fn presence_map<F>(proposal: &Value, category: &str, key_fn: F) -> BTreeMap<String, bool>
where
    F: Fn(&Map<String, Value>) -> Option<String>,
{
    let mut out = BTreeMap::new();
    if let Some(rows) = proposal.get(category).and_then(Value::as_array) {
        for e in rows {
            let Some(obj) = e.as_object() else {
                continue;
            };
            if let Some(k) = key_fn(obj) {
                if !k.is_empty() {
                    out.insert(k, true);
                }
            }
        }
    }
    out
}

/// A key agrees when every completed run assigns it the SAME value (a run
/// that omits the key contributes None, so "some propose, some don't" is a
/// disagreement). Generic over the map's value type: priority agreement
/// compares proposed values; the presence categories compare bools.
/// Returns {agree, total, disagreeing}.
fn category_agreement<V: PartialEq>(per_run_maps: Vec<BTreeMap<String, V>>) -> Value {
    let mut universe: BTreeSet<String> = BTreeSet::new();
    for m in &per_run_maps {
        universe.extend(m.keys().cloned());
    }
    let mut agree = 0i64;
    let mut disagreeing: Vec<String> = Vec::new();
    for k in &universe {
        let vals: Vec<Option<&V>> = per_run_maps.iter().map(|m| m.get(k)).collect();
        let first = vals[0];
        if vals.iter().all(|v| *v == first) {
            agree += 1;
        } else {
            disagreeing.push(k.clone());
        }
    }
    json!({"agree": agree, "total": universe.len(), "disagreeing": disagreeing})
}

/// Per-category agreement over the COMPLETED-run proposals. Priority is
/// keyed by node id plus `to` value; the rest are presence-based.
pub fn fold_consistency(proposals: &[Value]) -> Value {
    let dep_key = |obj: &Map<String, Value>| -> Option<String> {
        let from = obj.get("from").map(|v| v.to_string()).unwrap_or_default();
        let to = obj.get("to").map(|v| v.to_string()).unwrap_or_default();
        Some(format!("{from}->{to}"))
    };
    let defer_key =
        |obj: &Map<String, Value>| -> Option<String> { obj.get("id").map(|v| v.to_string()) };
    let dup_key = |obj: &Map<String, Value>| -> Option<String> {
        let mut parts: Vec<String> = obj
            .get("ids")?
            .as_array()?
            .iter()
            .map(|v| v.to_string())
            .collect();
        parts.sort();
        Some(parts.join(","))
    };
    json!({
        "priority": category_agreement(proposals.iter().map(priority_map).collect()),
        "dependencies": category_agreement(
            proposals
                .iter()
                .map(|p| presence_map(p, "dependencies", dep_key))
                .collect(),
        ),
        "defer": category_agreement(
            proposals
                .iter()
                .map(|p| presence_map(p, "defer", defer_key))
                .collect(),
        ),
        "duplicates": category_agreement(
            proposals
                .iter()
                .map(|p| presence_map(p, "duplicates", dup_key))
                .collect(),
        ),
    })
}

/// Best-effort `triage_applied` telemetry. The graph mutation has already
/// committed by the time this runs; an emit failure logs one stderr line
/// and never changes apply semantics or the exit code.
pub fn emit_triage_applied(applied: &Value, priority_moves: &[Value], proposed: i64, dropped: i64) {
    let Some(path) = super::advance::advance_events_path(None) else {
        return;
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let emitter = crate::events::EventEmitter::new(path, "backlog");
    let _ = emitter.emit_fields("triage_applied", {
        let mut m = Map::new();
        m.insert("applied".to_string(), applied.clone());
        m.insert(
            "priority_moves".to_string(),
            Value::Array(priority_moves.to_vec()),
        );
        m.insert("proposed".to_string(), json!(proposed));
        m.insert("dropped".to_string(), json!(dropped));
        m
    });
}

/// One inbox (fu-) item the context carries beside graph candidates:
/// `{id, id_type: "fu", title, priority, source: "inbox"}` (triage.py
/// _collect_inbox_items). The `fu-` token and `(pN)` suffix grammar is the
/// capture tier's line format; only OPEN checkbox rows count.
pub fn collect_inbox_items() -> Vec<Value> {
    let Some(path) = inbox_markdown_path() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(item) = parse_inbox_line(line) else {
            continue;
        };
        if item.0 != "open" {
            continue;
        }
        out.push(json!({
            "id": item.1,
            "id_type": "fu",
            "title": item.2,
            "priority": item.3,
            "source": "inbox",
        }));
    }
    out
}

/// The capture-tier inbox markdown (fno.paths.inbox_path): the config pin,
/// the parking-lot unification, then the vault and legacy fallbacks.
pub fn inbox_markdown_path() -> Option<std::path::PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    let root = crate::paths::worktree_repo_root(&cwd);
    // 1. config.paths.inbox_path, 2. config.post_merge.parking_lot_path.
    for keys in [["paths", "inbox_path"], ["post_merge", "parking_lot_path"]] {
        if let Some(pin) = crate::agents_config::config_lookup(&cwd, &keys) {
            let raw = pin.as_str().unwrap_or_default();
            if !raw.is_empty() {
                return Some(std::path::PathBuf::from(
                    super::super::territory::normalize_path(raw),
                ));
            }
        }
    }
    // 3. the vault default when internal/ exists, legacy inbox.md keeping
    // its claim; 4/5. the legacy files alone; 6. the parking-lot default.
    let internal = root.join("internal");
    let vault_inbox = internal.join("fno").join("backlog").join("parking-lot.md");
    let legacy_internal = internal.join("fno").join("backlog").join("inbox.md");
    let legacy_dotfno = root.join(".fno").join("backlog").join("inbox.md");
    if internal.is_dir() {
        if legacy_internal.is_file() {
            return Some(legacy_internal);
        }
        return Some(vault_inbox);
    }
    if legacy_internal.is_file() {
        return Some(legacy_internal);
    }
    if legacy_dotfno.is_file() {
        return Some(legacy_dotfno);
    }
    Some(root.join(".fno").join("backlog").join("parking-lot.md"))
}

/// One `- [mark] fu-token <sep> title` line -> (status, id, title,
/// priority). The em-dash and hyphen separators both parse; a trailing
/// `(pN)` splits off as the priority. None on any shape mismatch.
fn parse_inbox_line(line: &str) -> Option<(String, String, String, Option<String>)> {
    let rest = line.strip_prefix("- [")?;
    let mut parts = rest.splitn(2, ']');
    let mark = parts.next()?;
    let after = parts.next()?.strip_prefix(' ')?;
    if mark.len() != 1 {
        return None;
    }
    let status = match mark {
        " " => "open",
        "x" => "promoted",
        "-" => "dismissed",
        _ => return None,
    };
    let mut tokens = after.splitn(3, ' ');
    let fu_id = tokens.next()?;
    if !fu_id.starts_with("fu-") || fu_id.len() < 4 {
        return None;
    }
    let body = fu_id[3..].to_string();
    if body.is_empty()
        || !body
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        || !body
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return None;
    }
    let sep = tokens.next()?;
    if sep != "-" && sep != "\u{2014}" {
        return None;
    }
    let title_raw = tokens.next()?;
    let (title, priority) = split_priority_suffix(title_raw);
    Some((status.to_string(), format!("fu-{body}"), title, priority))
}

/// `title (p2)` -> (`title`, Some("p2")); no suffix -> (raw, None).
fn split_priority_suffix(raw: &str) -> (String, Option<String>) {
    let trimmed = raw.trim_end();
    let mut chars = trimmed.char_indices().rev();
    let close = chars.find(|(_, c)| *c == ')').map(|(i, _)| i);
    if let Some(close) = close {
        let open = trimmed[..close].rfind('(');
        if let Some(open) = open {
            let candidate = &trimmed[open + 1..close];
            if candidate.len() == 2
                && candidate.starts_with('p')
                && candidate[1..].chars().all(|c| c.is_ascii_digit())
            {
                return (
                    trimmed[..open].trim_end().to_string(),
                    Some(candidate.to_string()),
                );
            }
        }
    }
    (raw.trim().to_string(), None)
}

/// Project goals from the config chain (triage.py _load_goals):
/// `project.goals` first, then a legacy top-level `goals:` block, else
/// empty. Normalized to the id/goal/status keys the reasoning prompt uses.
pub fn load_goals() -> Vec<Value> {
    let cwd = std::env::current_dir().unwrap_or_default();
    for keys in [["project", "goals"], ["goals"]] {
        if let Some(goals) = crate::agents_config::config_lookup(&cwd, &keys) {
            let Some(rows) = goals.as_array() else {
                continue;
            };
            let normalized: Vec<Value> = rows
                .iter()
                .filter(|g| g.is_object())
                .map(|g| {
                    let mut m = Map::new();
                    for key in ["id", "goal", "status"] {
                        if let Some(v) = g.get(key) {
                            m.insert(key.to_string(), v.clone());
                        }
                    }
                    Value::Object(m)
                })
                .collect();
            if !normalized.is_empty() {
                return normalized;
            }
        }
    }
    Vec::new()
}

/// The LLM-reasoning context payload (triage.py _build_context), shared by
/// context and consistency so both reason over one identical snapshot.
pub fn build_context(
    deep: bool,
    all_projects: bool,
    project: Option<&str>,
    roadmap_id: Option<&str>,
) -> Result<Value, String> {
    let entries = triage_entries()?;
    let scoped = filter_by_project(&entries, project, all_projects);
    let candidates = collect_candidates(&scoped, roadmap_id, deep, false);
    let ideas = collect_candidates(&scoped, roadmap_id, deep, true);
    let inbox_items = collect_inbox_items();
    let goals = load_goals();
    Ok(json!({
        "candidates": candidates,
        "ideas": ideas,
        "inbox_items": inbox_items,
        "goals": goals,
        "mode": if deep { "deep" } else { "shallow" },
        "count": candidates.len(),
        "idea_count": ideas.len(),
        "inbox_count": inbox_items.len(),
        "scope": resolve_scope(project, all_projects, &entries),
    }))
}

/// The pending or idea collector, scoped and priority-sorted, projected to
/// candidate records (triage.py _collect_pending / _collect_ideas).
pub fn collect_candidates(
    scoped: &[Value],
    roadmap_id: Option<&str>,
    deep: bool,
    idea: bool,
) -> Vec<Value> {
    let mut picked: Vec<Value> = scoped
        .iter()
        .filter(|e| {
            e.get("roadmap_id").and_then(Value::as_str) == roadmap_id.filter(|r| !r.is_empty())
                || roadmap_id.is_none_or(|r| r.is_empty())
        })
        .filter(|e| if idea { is_idea(e) } else { is_pending(e) })
        .cloned()
        .collect();
    sort_entries_by_priority_created(&mut picked);
    picked.iter().map(|e| candidate_record(e, deep)).collect()
}
