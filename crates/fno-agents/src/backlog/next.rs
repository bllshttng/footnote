//! The selection door (`fno backlog next`), native on the graph backend:
//! prelude, occupancy, the keeper's select in-process, the observer merge
//! with its guard re-filter, the claims-store reservation, and the
//! zero-silent-starvation receipts. The external-backend shape (the joined
//! tracker candidates) still rides the python wheel, the same split the
//! `get`, `find`, and `undispatched` arms make.

use crate::backlog_ready::{
    configured_staleness_days, descendants_of, detect_project, dispatch_node_summary,
    find_node_index, select, selection_guards, truthy, NoSuchParent, ReadyOpts,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

const EXTERNAL_SELECTION_TTL: &str = "15m";
const USAGE: &str = "usage: fno-agents backlog next [--project P] [--all] [--ideas] [--include-deferred] [--roadmap-id R] [--parent ID] [--mission M] [--claim SESSION]";

struct Opts {
    roadmap_id: Option<String>,
    parent: Option<String>,
    claim: Option<String>,
    project: Option<String>,
    all: bool,
    include_ideas: bool,
    include_deferred: bool,
    mission: Option<String>,
}

fn parse_opts(args: &[String]) -> Option<Opts> {
    let mut o = Opts {
        roadmap_id: None,
        parent: None,
        claim: None,
        project: None,
        all: false,
        include_ideas: false,
        include_deferred: false,
        mission: None,
    };
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        let mut flag_value = |slot: &mut Option<String>| -> bool {
            match args.get(index + 1) {
                Some(v) => {
                    *slot = Some(v.clone());
                    index += 1;
                    true
                }
                None => false,
            }
        };
        match arg {
            "--roadmap-id" => {
                if !flag_value(&mut o.roadmap_id) {
                    return None;
                }
            }
            "--parent" => {
                if !flag_value(&mut o.parent) {
                    return None;
                }
            }
            "--claim" => {
                if !flag_value(&mut o.claim) {
                    return None;
                }
            }
            "--project" | "-p" => {
                if !flag_value(&mut o.project) {
                    return None;
                }
            }
            "--mission" => {
                if !flag_value(&mut o.mission) {
                    return None;
                }
            }
            "--all" | "-A" => o.all = true,
            "--ideas" | "-I" | "--include-ideas" => o.include_ideas = true,
            "--include-deferred" => o.include_deferred = true,
            _ => return None,
        }
        index += 1;
    }
    Some(o)
}

fn live_claimed_node_ids() -> Result<BTreeSet<String>, String> {
    match crate::claims::list(Some("node:"), None, false) {
        Ok(records) => Ok(records
            .iter()
            .filter_map(|rec| rec.key.strip_prefix("node:").map(str::to_string))
            .collect()),
        Err(error) => Err(format!("live claim state is unavailable ({error})")),
    }
}

/// Terminal-strand liveness (`strand._is_live`): the recompute floor via its
/// own fields; an unverified supersession stays live.
fn is_live_entry(entry: &Value) -> bool {
    if truthy(entry.get("completed_at")) || truthy(entry.get("deferred_at")) {
        return false;
    }
    if !truthy(entry.get("superseded_by")) {
        return true;
    }
    entry
        .get("supersession")
        .and_then(Value::as_object)
        .is_some_and(|s| !truthy(s.get("verified_at")))
}

fn first_dead_ancestor(
    entry: &Value,
    by_id: &BTreeMap<String, Value>,
    is_dead: impl Fn(&Value) -> bool,
) -> Option<String> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut cur = entry
        .get("parent")
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut steps = 0usize;
    while let Some(id) = cur {
        if steps >= 64 {
            break;
        }
        steps += 1;
        if !seen.insert(id.clone()) {
            break;
        }
        let Some(anc) = by_id.get(&id) else {
            break;
        };
        if is_dead(anc) {
            return Some(id);
        }
        cur = anc
            .get("parent")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    None
}

fn has_unmerged_open_pr(entry: &Value) -> bool {
    if truthy(entry.get("completed_at")) {
        return false;
    }
    truthy(entry.get("pr_number"))
}

fn container_ids(entries: &[Value]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for e in entries {
        if let (Some(p), Some(parent)) = (
            e.get("parent").and_then(Value::as_str),
            e.get("contained_in").and_then(Value::as_str),
        ) {
            if p != parent {
                out.insert(p.to_string());
            }
        } else if let Some(p) = e.get("parent").and_then(Value::as_str) {
            out.insert(p.to_string());
        }
    }
    out
}

/// Why each ready-ish in-scope node was NOT selected: the G1 receipts. The
/// reason ladder is the python classifier's, decision for decision.
#[allow(clippy::too_many_arguments)]
fn starvation_receipts(
    entries: &[Value],
    project_filter: Option<&str>,
    all: bool,
    scope_ids: Option<&BTreeSet<String>>,
    claimed: &BTreeSet<String>,
    now_ms: i64,
    staleness_days: i64,
    held: &BTreeMap<String, String>,
    mission: Option<&str>,
    roadmap_id: Option<&str>,
) -> Vec<(String, String)> {
    let containers = container_ids(entries);
    let by_id: BTreeMap<String, Value> = entries
        .iter()
        .filter_map(|e| {
            e.get("id")
                .and_then(Value::as_str)
                .map(|id| (id.to_string(), e.clone()))
        })
        .collect();
    let mut ready_ish: Vec<&Value> = Vec::new();
    for e in entries {
        let Some(status) = e.get("status").and_then(Value::as_str) else {
            continue;
        };
        if !matches!(status, "ready" | "design" | "idea") || truthy(e.get("completed_at")) {
            continue;
        }
        if let Some(roadmap) = roadmap_id {
            if e.get("roadmap_id").and_then(Value::as_str) != Some(roadmap) {
                continue;
            }
        }
        if let Some(mission) = mission {
            if e.get("mission_id").and_then(Value::as_str) != Some(mission) {
                continue;
            }
        }
        ready_ish.push(e);
    }
    ready_ish.retain(|e| match (project_filter, all) {
        (_, true) | (None, _) => true,
        (Some(p), false) => e.get("project").and_then(Value::as_str) == Some(p),
    });
    if let Some(scope) = scope_ids {
        ready_ish.retain(|e| {
            e.get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| scope.contains(id))
        });
    }
    let mut out: Vec<(String, String)> = Vec::new();
    for e in ready_ish {
        let Some(nid) = e.get("id").and_then(Value::as_str) else {
            continue;
        };
        let hold_guard = selection_guards(e, &by_id, now_ms, staleness_days, held);
        let status = e.get("status").and_then(Value::as_str).unwrap_or("");
        let reason: Option<String> = if hold_guard
            .as_deref()
            .is_some_and(|g| g.starts_with("dispatch-hold"))
        {
            hold_guard
        } else if first_dead_ancestor(e, &by_id, |anc| !is_live_entry(anc)).is_some()
            && !truthy(e.get("contained_in"))
            && !has_unmerged_open_pr(e)
            && !truthy(e.get("batch"))
        {
            Some("dead-ancestor".to_string())
        } else if !truthy(e.get("plan_path")) {
            Some("plan-less".to_string())
        } else if containers.contains(nid) {
            Some("container".to_string())
        } else if claimed.contains(nid) {
            Some("claimed".to_string())
        } else if matches!(status, "design" | "idea") {
            Some(status.to_string())
        } else if status == "ready" && (has_unmerged_open_pr(e) || truthy(e.get("batch"))) {
            continue;
        } else {
            match hold_guard.as_deref() {
                None => continue,
                Some(g) if g.starts_with("contained") => Some("contained".to_string()),
                Some("design-stage") => Some("design".to_string()),
                Some("idea-stage") => Some("idea".to_string()),
                Some(_) => Some("quarantined".to_string()),
            }
        };
        if let Some(reason) = reason {
            out.push((nid.to_string(), reason));
        }
    }
    out
}

/// Selector-only safety guards reapplied to the observer's rows, the
/// observer-only survivors prepended, and one divergence event per missed
/// row (non-gating).
#[allow(clippy::too_many_arguments)]
pub(crate) fn with_observer(
    candidates: Vec<Value>,
    source_entries: &[Value],
    occupied: &BTreeSet<String>,
    observer: &Value,
    project_filter: Option<&str>,
    all: bool,
    mission: Option<&str>,
    roadmap_id: Option<&str>,
    now_ms: i64,
    staleness_days: i64,
    held: &BTreeMap<String, String>,
    selector_command: &str,
) -> Vec<Value> {
    let by_id: BTreeMap<String, Value> = source_entries
        .iter()
        .filter_map(|e| {
            e.get("id")
                .and_then(Value::as_str)
                .map(|id| (id.to_string(), e.clone()))
        })
        .collect();
    let containers = container_ids(source_entries);
    let mut safe_rows: Vec<Value> = Vec::new();
    for row in observer
        .get("rows")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(id) = row.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(entry) = by_id.get(id) else {
            continue;
        };
        if occupied.contains(id) {
            continue;
        }
        if truthy(entry.get("completed_at")) || has_unmerged_open_pr(entry) {
            continue;
        }
        if containers.contains(id) || truthy(entry.get("batch")) {
            continue;
        }
        if selection_guards(entry, &by_id, now_ms, staleness_days, held).is_some() {
            continue;
        }
        safe_rows.push(entry.clone());
    }
    let scanned = observer
        .get("entries_scanned")
        .cloned()
        .unwrap_or_else(|| json!(0));
    let safe_observer = json!({ "rows": safe_rows, "entries_scanned": scanned });
    let (merged, missed) =
        crate::backlog::undispatched::prepend_missed_rows(candidates, &safe_observer);
    if !missed.is_empty() {
        let mut scope = format!(
            "project={}",
            if all {
                "*".to_string()
            } else {
                project_filter.unwrap_or("").to_string()
            }
        );
        if let Some(mission) = mission {
            scope.push_str(&format!(",mission={mission}"));
        }
        if let Some(roadmap) = roadmap_id {
            scope.push_str(&format!(",roadmap={roadmap}"));
        }
        for row in &missed {
            let Some(node_id) = row.get("id").and_then(Value::as_str) else {
                continue;
            };
            crate::backlog::workflows::emit_event(
                "dispatch_selection_diverged",
                json!({
                    "node_id": node_id,
                    "selector_command": selector_command,
                    "observer_command": "fno backlog undispatched --json",
                    "scope": scope,
                    "selector_entries_scanned": 0,
                    "observer_entries_scanned": scanned,
                }),
            );
        }
    }
    merged
}

fn classify_observer(
    entries: &[Value],
    claimed: &BTreeSet<String>,
    worked: &[(String, Vec<String>)],
    o: &Opts,
    project_filter: &Option<String>,
    parent_target_id: &Option<String>,
) -> Result<Value, String> {
    let mut claims: Vec<Value> = claimed
        .iter()
        .map(|id| json!({"key": format!("node:{id}"), "state": Value::Null}))
        .collect();
    for (node_id, _workers) in worked {
        claims.push(json!({"key": format!("node:{node_id}"), "state": "live-worker"}));
    }
    crate::backlog::undispatched::classify_planned_unclaimed(
        entries,
        &claims,
        if o.all {
            None
        } else {
            project_filter.as_deref()
        },
        o.mission.as_deref(),
        o.roadmap_id.as_deref(),
        parent_target_id.as_deref(),
    )
}

fn select_rows(
    entries: &[Value],
    o: &Opts,
    project_filter: &Option<String>,
    parent_target_id: &Option<String>,
    repo_root: &str,
    occupied: &BTreeSet<String>,
    staleness_days: i64,
    now_ms: i64,
    held: &BTreeMap<String, String>,
) -> Result<Vec<Value>, String> {
    let opts = ReadyOpts {
        project: project_filter.clone(),
        all: o.all,
        roadmap_id: o.roadmap_id.clone(),
        parent: parent_target_id.clone(),
        mission: o.mission.clone(),
        include_ideas: o.include_ideas,
        include_deferred: o.include_deferred,
        repo_root: Some(repo_root.to_string()),
        claimed: occupied.clone(),
        held: held.clone(),
        staleness_days: Some(staleness_days),
        now_ms,
    };
    match select(entries, &opts) {
        Ok(reply) => Ok(reply.rows),
        Err(NoSuchParent(parent)) => Err(format!("no such node '{parent}'")),
    }
}

fn stranded_next_receipts(receipts: &[(String, String)]) -> Vec<String> {
    let stranded: Vec<&(String, String)> = receipts
        .iter()
        .filter(|(_, r)| r == "dead-ancestor")
        .collect();
    if stranded.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<String> = stranded
        .iter()
        .take(10)
        .map(|(nid, r)| format!("stranded {nid}: {r}"))
        .collect();
    let shown = if stranded.len() > 10 {
        format!(" (showing 10)")
    } else {
        String::new()
    };
    lines.push(format!(
        "{} node(s) stranded under terminal parents{shown}; `fno backlog reconcile` re-parents them",
        stranded.len()
    ));
    lines
}

pub fn run(args: &[String]) -> i32 {
    let Some(o) = parse_opts(args) else {
        eprintln!("{USAGE}");
        return 2;
    };
    if crate::backlog::workflows::active_backend_name() != "graph" {
        return crate::backlog::cli::forward_to_python("next", args);
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let repo_root = crate::backlog::create_cli::repo_root(&cwd);
    let now_ms = crate::claims::now_ms();
    let config_dir = crate::claude_roster::config_dir();
    let staleness_days = configured_staleness_days(&config_dir).unwrap_or(21).max(1);
    let fno_dir = super::settings::graph_path()
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| cwd.clone());
    let held = crate::needs::held_map(&fno_dir, &cwd);

    // One strict read for the prelude AND selection: the working graph, read
    // at most once for project detection AND parent resolution.
    let need_prelude = (o.project.is_none() && !o.all) || o.parent.is_some() || o.claim.is_some();
    let pre_entries = if need_prelude {
        let graph = super::settings::graph_path();
        match crate::graph_store::read_rows_strict(&graph) {
            Ok(rows) => Some(rows),
            Err(error) => {
                eprintln!("Error: graph unreadable: {error}; selection refused");
                return 1;
            }
        }
    } else {
        None
    };
    let mut project_filter = o.project.clone();
    if project_filter.is_none() && !o.all {
        let Some(entries) = &pre_entries else {
            eprintln!("Error: graph unreadable: the prelude is empty; selection refused");
            return 1;
        };
        project_filter = detect_project(entries, &repo_root);
    }
    let mut parent_target_id: Option<String> = None;
    if let Some(parent) = &o.parent {
        let Some(entries) = &pre_entries else {
            eprintln!("Error: graph unreadable: the prelude is empty; selection refused");
            return 1;
        };
        let (idx, _fuzzy) = find_node_index(entries, parent);
        let Some(idx) = idx else {
            eprintln!("Error: no such node '{parent}'");
            return 1;
        };
        let target_id = entries[idx]
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or(parent)
            .to_string();
        if descendants_of(entries, &target_id).is_empty() {
            eprintln!("no children under {target_id}");
        }
        parent_target_id = Some(target_id);
    }

    // Dispatch occupancy plus the observer receipt, read ONCE per selection.
    let prepare = |entries: &[Value]| -> Result<(BTreeSet<String>, Value), String> {
        let claimed = live_claimed_node_ids()?;
        let worked = crate::backlog::worked::live_worked_node_ids(entries)?;
        let observer = classify_observer(
            entries,
            &claimed,
            &worked,
            &o,
            &project_filter,
            &parent_target_id,
        )?;
        let mut occupied = claimed.clone();
        for (node_id, _workers) in &worked {
            occupied.insert(node_id.clone());
        }
        Ok((occupied, observer))
    };
    let mut result: Option<Value> = None;
    let selection_claimed: BTreeSet<String>;
    let entries_for_receipts: Vec<Value>;
    if let Some(claim) = &o.claim {
        let Some(pre) = &pre_entries else {
            eprintln!("null");
            return 0;
        };
        let (occupied, observer) = match prepare(pre) {
            Ok(v) => v,
            Err(reason) => {
                eprintln!("Error: {reason}; selection refused");
                return 1;
            }
        };
        selection_claimed = occupied.clone();
        let candidates = match select_rows(
            pre,
            &o,
            &project_filter,
            &parent_target_id,
            &repo_root,
            &occupied,
            staleness_days,
            now_ms,
            &held,
        ) {
            Ok(rows) => rows,
            Err(reason) => {
                eprintln!("Error: {reason}");
                return 1;
            }
        };
        let merged = with_observer(
            candidates,
            pre,
            &occupied,
            &observer,
            project_filter.as_deref(),
            o.all,
            o.mission.as_deref(),
            o.roadmap_id.as_deref(),
            now_ms,
            staleness_days,
            &held,
            "fno backlog next",
        );
        let ttl = crate::claims::parse_ttl_ms(EXTERNAL_SELECTION_TTL);
        for winner in &merged {
            let Some(node_id) = winner.get("id").and_then(Value::as_str) else {
                continue;
            };
            let key = format!("node:{node_id}");
            let opts = crate::claims::AcquireOpts {
                ttl_ms: ttl,
                ..Default::default()
            };
            match crate::claims::acquire(&key, claim, opts) {
                crate::claims::AcquireOutcome::Acquired(_) => {
                    result = Some(dispatch_node_summary(winner));
                    break;
                }
                crate::claims::AcquireOutcome::HeldByOther { .. } => continue,
                crate::claims::AcquireOutcome::Error(error) => {
                    eprintln!("Error: claim acquire failed: {error}; selection refused");
                    return 1;
                }
            }
        }
        let graph = super::settings::graph_path();
        entries_for_receipts = crate::graph_store::read_rows(&graph).unwrap_or_default();
    } else {
        let entries = match pre_entries {
            Some(entries) => entries,
            None => {
                let graph = super::settings::graph_path();
                match crate::graph_store::read_rows_strict(&graph) {
                    Ok(rows) => rows,
                    Err(error) => {
                        eprintln!("Error: graph unreadable: {error}; selection refused");
                        return 1;
                    }
                }
            }
        };
        let (occupied, observer) = match prepare(&entries) {
            Ok(v) => v,
            Err(reason) => {
                eprintln!("Error: {reason}; selection refused");
                return 1;
            }
        };
        selection_claimed = occupied.clone();
        let candidates = match select_rows(
            &entries,
            &o,
            &project_filter,
            &parent_target_id,
            &repo_root,
            &occupied,
            staleness_days,
            now_ms,
            &held,
        ) {
            Ok(rows) => rows,
            Err(reason) => {
                eprintln!("Error: {reason}");
                return 1;
            }
        };
        let merged = with_observer(
            candidates,
            &entries,
            &occupied,
            &observer,
            project_filter.as_deref(),
            o.all,
            o.mission.as_deref(),
            o.roadmap_id.as_deref(),
            now_ms,
            staleness_days,
            &held,
            "fno backlog next",
        );
        if let Some(first) = merged.first() {
            result = Some(dispatch_node_summary(first));
        }
        entries_for_receipts = entries;
    }

    // Zero-silent-starvation receipts: advisory stderr, never breaking the
    // stdout contract.
    let scope_ids: Option<BTreeSet<String>> = parent_target_id
        .as_ref()
        .map(|pid| descendants_of(&entries_for_receipts, pid));
    let receipts = starvation_receipts(
        &entries_for_receipts,
        project_filter.as_deref(),
        o.all,
        scope_ids.as_ref(),
        &selection_claimed,
        now_ms,
        staleness_days,
        &held,
        o.mission.as_deref(),
        o.roadmap_id.as_deref(),
    );
    match &result {
        None => {
            for (nid, reason) in &receipts {
                eprintln!("excluded {nid}: {reason}");
            }
        }
        Some(_) => {
            for line in stranded_next_receipts(&receipts) {
                eprintln!("{line}");
            }
        }
    }
    match result {
        Some(summary) => println!(
            "{}",
            serde_json::to_string_pretty(&summary).unwrap_or_else(|_| "null".into())
        ),
        None => println!("null"),
    }
    0
}
