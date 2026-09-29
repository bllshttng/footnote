//! The planned-unclaimed observer (`fno backlog undispatched`), native on
//! the graph backend: one classify over the graph, the claim keys, and the
//! worked fold, ordered through the one selection key. The external-backend
//! shape (the joined tracker candidates) still rides the python wheel, the
//! same split the `get` and `find` arms make.

use crate::backlog_ready::{
    dependents_fanout, epics_with_child_progress, in_progress_epic_ids, make_effective_priority,
    orphan_ids, selection_sort_key, truthy,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

const OBSERVER_COMMAND: &str = "fno backlog undispatched --json";

const USAGE: &str =
    "usage: fno-agents backlog undispatched [--project P] [--roadmap-id R] [--parent ID] [--mission M]";

/// Transitive children of `parent_id` over the `parent` edge.
fn descendants(entries: &[Value], parent_id: &str) -> BTreeSet<String> {
    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for entry in entries {
        if let (Some(id), Some(parent)) = (
            entry.get("id").and_then(Value::as_str),
            entry.get("parent").and_then(Value::as_str),
        ) {
            children
                .entry(parent.to_string())
                .or_default()
                .push(id.to_string());
        }
    }
    let mut found = BTreeSet::new();
    let mut frontier: Vec<String> = children.get(parent_id).cloned().unwrap_or_default();
    while let Some(id) = frontier.pop() {
        if !found.insert(id.clone()) {
            continue;
        }
        frontier.extend(children.get(&id).cloned().unwrap_or_default());
    }
    found
}

fn has_pr(entry: &Value) -> bool {
    if truthy(entry.get("pr_number")) {
        return true;
    }
    entry
        .get("additional_prs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|extra| extra.is_object() && truthy(extra.get("number")))
}

fn blocked(entry: &Value, by_id: &BTreeMap<String, Value>) -> bool {
    for blocker_id in entry
        .get("blocked_by")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        match by_id.get(blocker_id) {
            None => return true,
            Some(blocker) => {
                let done = blocker.get("status").and_then(Value::as_str) == Some("done")
                    || truthy(blocker.get("completed_at"));
                if !done {
                    return true;
                }
            }
        }
    }
    false
}

/// The one observer receipt: planned, finalized, leaf, unclaimed rows in
/// selection order. Errors name the failed source, exactly the python
/// ValueError wordings the caller wraps into ObserverReadError.
#[allow(clippy::too_many_arguments)]
pub(crate) fn classify_planned_unclaimed(
    entries: &[Value],
    claims: &[Value],
    project: Option<&str>,
    mission: Option<&str>,
    roadmap_id: Option<&str>,
    parent: Option<&str>,
) -> Result<Value, String> {
    let by_id: BTreeMap<String, Value> = entries
        .iter()
        .filter_map(|e| {
            e.get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(|id| (id.to_string(), e.clone()))
        })
        .collect();
    let mut claimed: BTreeMap<String, String> = BTreeMap::new();
    for claim in claims {
        let Some(key) = claim.get("key").and_then(Value::as_str) else {
            return Err("claims unreadable: claim key is not a string".to_string());
        };
        if let Some(node) = key.strip_prefix("node:") {
            claimed.insert(
                node.to_string(),
                claim
                    .get("state")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
            );
        }
    }

    let child_ids: BTreeSet<&str> = entries
        .iter()
        .filter_map(|e| e.get("parent").and_then(Value::as_str))
        .collect();
    let scope = parent.map(|p| descendants(entries, p));
    let mut rows: Vec<Value> = Vec::new();
    for entry in entries {
        let Some(node_id) = entry.get("id").and_then(Value::as_str) else {
            return Err("graph unreadable: entry id is not a string".to_string());
        };
        if node_id.is_empty() {
            return Err("graph unreadable: entry id is not a string".to_string());
        }
        if let Some(project) = project {
            if entry.get("project").and_then(Value::as_str) != Some(project) {
                continue;
            }
        }

        if let Some(mission) = mission {
            if entry.get("mission_id").and_then(Value::as_str) != Some(mission) {
                continue;
            }
        }
        if let Some(roadmap_id) = roadmap_id {
            if entry.get("roadmap_id").and_then(Value::as_str) != Some(roadmap_id) {
                continue;
            }
        }
        if let Some(scope) = &scope {
            if !scope.contains(node_id) {
                continue;
            }
        }
        let claim_state = claimed.get(node_id).cloned();
        let status_ready = entry.get("status").and_then(Value::as_str) == Some("ready");
        let plan_finalized = entry
            .get("plan_path")
            .and_then(Value::as_str)
            .is_some_and(|p| !p.trim().is_empty());
        let leaf = entry.get("type").and_then(Value::as_str) != Some("epic")
            && !child_ids.contains(node_id);
        let completed = truthy(entry.get("completed_at"));
        let has_pr = has_pr(entry);
        let batch_owner = truthy(entry.get("batch"));
        let blocked = blocked(entry, &by_id);
        let contained = truthy(entry.get("contained_in"));
        let offerable = status_ready
            && plan_finalized
            && leaf
            && !completed
            && !has_pr
            && !batch_owner
            && !blocked
            && !contained
            && claim_state.is_none();
        if !offerable {
            continue;
        }
        let facts = json!({
            "status_ready": status_ready,
            "plan_finalized": plan_finalized,
            "leaf": leaf,
            "completed": completed,
            "has_pr": has_pr,
            "batch_owner": batch_owner,
            "blocked": blocked,
            "contained": contained,
            "claim_state": claim_state,
        });
        let mut row = json!({
            "id": node_id,
            "priority": entry.get("priority"),
            "domain": entry.get("domain"),
            "plan_path": entry.get("plan_path"),
            "facts": facts,
        });
        if let Some(obj) = row.as_object_mut() {
            for key in [
                "title",
                "project",
                "mission_id",
                "roadmap_id",
                "parent",
                "dispatch_verb",
                "dispatch_brief",
            ] {
                let value = entry.get(key).filter(|v| !v.is_null());
                if let Some(value) = value {
                    obj.insert(key.to_string(), value.clone());
                }
            }
        }
        rows.push(row);
    }

    // One ordering for both queues: the shared selection key over the FULL
    // graph, with the row id as the textual tiebreak (the python tuple's
    // second term). Live claims feed the epic-progress term only.
    let live: BTreeSet<String> = claimed
        .iter()
        .filter(|(_, state)| state.as_str() == "live")
        .map(|(id, _)| id.clone())
        .collect();
    let child_progress = epics_with_child_progress(&by_id);
    let dependents = dependents_fanout(entries);
    let effective_priority = make_effective_priority(&by_id, &child_progress);
    let orphans = orphan_ids(entries, &by_id);
    let epic_in_progress = in_progress_epic_ids(entries, &by_id, &child_progress, &live);
    let now_ms = crate::claims::now_ms();
    let mut keyed: Vec<(Vec<crate::backlog_ready::Term>, String, Value)> = rows
        .into_iter()
        .map(|row| {
            let id = row
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let entry = by_id.get(&id).cloned().unwrap_or(Value::Null);
            let key = selection_sort_key(
                &entry,
                &by_id,
                &child_progress,
                &dependents,
                &effective_priority,
                &orphans,
                &epic_in_progress,
                now_ms,
            );
            (key, id.clone(), row)
        })
        .collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let rows: Vec<Value> = keyed.into_iter().map(|(_, _, row)| row).collect();
    Ok(json!({
        "source": OBSERVER_COMMAND,
        "status": "ok",
        "entries_scanned": entries.len(),
        "claims_scanned": claims.len(),
        "rows": rows,
    }))
}

/// Observer-only rows before the normal selector frontier: (merged, missed).
pub(crate) fn prepend_missed_rows(
    normal_rows: Vec<Value>,
    observer: &Value,
) -> (Vec<Value>, Vec<Value>) {
    let normal_ids: BTreeSet<String> = normal_rows
        .iter()
        .filter_map(|r| r.get("id").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    let mut missed: Vec<Value> = Vec::new();
    for row in observer
        .get("rows")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(id) = row.get("id").and_then(Value::as_str) else {
            continue;
        };
        if !normal_ids.contains(id) {
            missed.push(row.clone());
        }
    }
    let mut merged = missed.clone();
    merged.extend(normal_rows);
    (merged, missed)
}

/// The graph-backend door: graph, claims, worked fold, one classify.
pub fn run(args: &[String]) -> i32 {
    let mut project: Option<String> = None;
    let mut roadmap_id: Option<String> = None;
    let mut parent: Option<String> = None;
    let mut mission: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        let takes_value = matches!(
            arg,
            "--project" | "-p" | "--roadmap-id" | "--parent" | "--mission"
        );
        let ignored = matches!(arg, "--all" | "-A" | "--json" | "-J");
        let help = matches!(arg, "-h" | "--help");
        if help {
            println!("Name finalized, ready leaf plans with no node claim.\n\n{USAGE}");
            return 0;
        }
        if ignored {
            index += 1;
            continue;
        }
        if !takes_value {
            eprintln!("{USAGE}");
            return 2;
        }
        let Some(value) = args.get(index + 1) else {
            eprintln!("{USAGE}");
            return 2;
        };
        match arg {
            "--project" | "-p" => project = Some(value.clone()),
            "--roadmap-id" => roadmap_id = Some(value.clone()),
            "--parent" => parent = Some(value.clone()),
            "--mission" => mission = Some(value.clone()),
            _ => {
                eprintln!("{USAGE}");
                return 2;
            }
        }
        index += 2;
    }
    if crate::backlog::workflows::active_backend_name() != "graph" {
        return crate::backlog::cli::forward_to_python("undispatched", args);
    }

    let graph = super::settings::graph_path();
    let entries = match crate::graph_store::read_rows_strict(&graph) {
        Ok(rows) => rows,
        Err(error) => {
            eprintln!("Error: tracker unreadable: graph unreadable: {error}");
            return 1;
        }
    };
    // The claim snapshot is keys-only on the real path, so every state reads
    // unknown; the worked overlay rides in as live-worker synthetic claims.
    let claims = match crate::claims::list(Some("node:"), None, false) {
        Ok(records) => records
            .iter()
            .map(|rec| json!({"key": rec.key, "state": Value::Null}))
            .collect::<Vec<_>>(),
        Err(error) => {
            eprintln!("Error: claims unreadable: {error}");
            return 1;
        }
    };
    let worked = match crate::backlog::worked::live_worked_node_ids(&entries) {
        Ok(worked) => worked,
        Err(reason) => {
            eprintln!("Error: worked overlay unreadable: {reason}");
            return 1;
        }
    };
    let mut claims: Vec<Value> = claims;
    for (node_id, _workers) in &worked {
        claims.push(json!({"key": format!("node:{node_id}"), "state": "live-worker"}));
    }
    let receipt = match classify_planned_unclaimed(
        &entries,
        &claims,
        project.as_deref(),
        mission.as_deref(),
        roadmap_id.as_deref(),
        parent.as_deref(),
    ) {
        Ok(receipt) => receipt,
        Err(reason) => {
            eprintln!("Error: {reason}");
            return 1;
        }
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&receipt).unwrap_or_else(|_| "{}".into())
    );
    0
}
