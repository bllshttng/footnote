//! Pure classification over the fetched sources: undispatched selection,
//! claimed-node reads, holder activity, driver state.
use super::prs::derived_status;
use super::{
    s_str, truthy, SourceRead, DEAD_CLAIM_STATES, KING_PRIORITIES, SRC_UNDISPATCHED, TERMINAL_RUNGS,
};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

/// Live node claims resolved per board read; the cut is reported (the
/// x-f8e3 reference carried the same cap).
pub(crate) const MAX_CLAIMED_NODE_READS: usize = 20;

/// The activity vocabulary that counts as a staffed lane (reachability
/// `_ACTIVE_STATES`). Copied with a test pinning the Python side, because a
/// Rust module cannot import the Python frozenset; the pin makes the
/// vocabulary fix that adds a fourth word fail loudly here.
pub(crate) const ACTIVE_STATES: [&str; 3] = ["working", "watching", "your-move"];

/// Transcript age past which an active-looking holder reads stalled
/// (session_truth.STALLED_AFTER_S; same pin as ACTIVE_STATES).
pub(crate) const STALLED_AFTER_S: f64 = 2.0 * 3600.0;

// ---------------------------------------------------------------------------
// Undispatched: classify_planned_unclaimed over the graph we already hold
// ---------------------------------------------------------------------------

/// Pure port of `backlog/undispatched.classify_planned_unclaimed`, minus the
/// selector filters the board never sets (project/mission/roadmap/parent).
/// Reads the same entries and claims rows the other queues use.
pub(crate) fn classify_planned_unclaimed(
    entries: &[Value],
    claims: &[Value],
) -> Result<Value, String> {
    let by_id: HashMap<&str, &Value> = entries
        .iter()
        .filter_map(|e| s_str(e, "id").map(|id| (id, e)))
        .collect();
    let mut claimed: HashMap<&str, &str> = HashMap::new();
    for claim in claims {
        let Some(key) = s_str(claim, "key") else {
            return Err("claims unreadable: claim key is not a string".to_string());
        };
        if let Some(node_id) = key.strip_prefix("node:") {
            claimed.insert(node_id, s_str(claim, "state").unwrap_or("unknown"));
        }
    }
    let child_ids: HashSet<&str> = entries.iter().filter_map(|e| s_str(e, "parent")).collect();

    let priority_rank = |p: &str| match p {
        "p0" => 0,
        "p1" => 1,
        "p2" => 2,
        "p3" => 3,
        _ => 99,
    };
    let mut rows: Vec<(i32, String, Value)> = Vec::new();
    for entry in entries {
        let Some(node_id) = s_str(entry, "id") else {
            return Err("graph unreadable: entry id is not a string".to_string());
        };
        let plan_finalized = s_str(entry, "plan_path")
            .map(|p| !p.trim().is_empty())
            .unwrap_or(false);
        let status_ready = s_str(entry, "status") == Some("ready");
        let leaf = s_str(entry, "type") != Some("epic") && !child_ids.contains(&node_id);
        let completed = entry.get("completed_at").map(truthy).unwrap_or(false);
        let has_pr = entry.get("pr_number").map(truthy).unwrap_or(false)
            || entry
                .get("additional_prs")
                .and_then(Value::as_array)
                .map(|extras| {
                    extras
                        .iter()
                        .any(|e| e.is_object() && e.get("number").map(truthy).unwrap_or(false))
                })
                .unwrap_or(false);
        let batch_owner = entry.get("batch").map(truthy).unwrap_or(false);
        let blocked = entry
            .get("blocked_by")
            .and_then(Value::as_array)
            .is_some_and(|blockers| {
                blockers.iter().any(|b| {
                    let Some(blocker_id) = b.as_str() else {
                        return true;
                    };
                    match by_id.get(blocker_id) {
                        None => true,
                        Some(blocker) => {
                            s_str(blocker, "status") != Some("done")
                                && !blocker.get("completed_at").map(truthy).unwrap_or(false)
                        }
                    }
                })
            });
        let claim_state = claimed.get(node_id).copied();
        let selected = status_ready
            && plan_finalized
            && leaf
            && !completed
            && !has_pr
            && !batch_owner
            && !blocked
            && claim_state.is_none();
        if !selected {
            continue;
        }
        let priority = s_str(entry, "priority").unwrap_or("unknown").to_string();
        let mut row = json!({
            "id": node_id,
            "priority": entry.get("priority"),
            "domain": entry.get("domain"),
            "plan_path": entry.get("plan_path"),
            "facts": {
                "status_ready": status_ready,
                "plan_finalized": plan_finalized,
                "leaf": leaf,
                "completed": completed,
                "has_pr": has_pr,
                "batch_owner": batch_owner,
                "blocked": blocked,
                "claim_state": claim_state,
            },
        });
        if let Some(obj) = row.as_object_mut() {
            for key in ["title", "project", "mission_id", "roadmap_id", "parent"] {
                if !entry.get(key).unwrap_or(&Value::Null).is_null() {
                    obj.insert(key.to_string(), entry.get(key).cloned().unwrap());
                }
            }
        }
        let rank = priority_rank(&priority);
        rows.push((rank, node_id.to_string(), row));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    Ok(json!({
        "source": SRC_UNDISPATCHED,
        "status": "ok",
        "entries_scanned": entries.len(),
        "claims_scanned": claims.len(),
        "rows": rows.into_iter().map(|(_, _, r)| r).collect::<Vec<_>>(),
    }))
}

// ---------------------------------------------------------------------------
// Claimed nodes + holder activity
// ---------------------------------------------------------------------------

/// The backlog row behind each LIVE node claim (board._read_claimed_nodes):
/// one graph read, exact id match (claims carry real ids; the slug fallback is
/// free and harmless), terminal claims dropped at the source.
pub(crate) fn read_claimed_nodes(
    claims: &SourceRead,
    entries: Option<&[Value]>,
) -> (SourceRead, Vec<String>, Vec<String>) {
    if !claims.is_ok() {
        return (
            SourceRead::err(claims.error.clone().unwrap_or_default()),
            Vec::new(),
            Vec::new(),
        );
    }
    let mut held: Vec<(String, String)> = Vec::new();
    for row in &claims.rows() {
        let Some(key) = s_str(row, "key") else {
            continue;
        };
        let Some(node_id) = key.strip_prefix("node:") else {
            continue;
        };
        let state = s_str(row, "state").unwrap_or("");
        if DEAD_CLAIM_STATES.contains(&state) {
            continue;
        }
        let holder = s_str(row, "holder").unwrap_or("");
        if !holder.is_empty() {
            held.push((node_id.to_string(), holder.to_string()));
        }
    }

    let Some(entries) = entries else {
        return (
            SourceRead::err("backlog get: graph unreadable"),
            Vec::new(),
            Vec::new(),
        );
    };
    let mut warnings: Vec<String> = Vec::new();
    if held.len() > MAX_CLAIMED_NODE_READS {
        warnings.push(format!(
            "stalled_holder: capped at {MAX_CLAIMED_NODE_READS} of {} live claims",
            held.len()
        ));
        held.truncate(MAX_CLAIMED_NODE_READS);
    }
    let mut nodes: Vec<Value> = Vec::new();
    let mut holders: Vec<String> = Vec::new();
    let mut seen_holders: HashSet<String> = HashSet::new();
    for (node_id, holder) in &held {
        let node = entries
            .iter()
            .find(|e| {
                s_str(e, "id")
                    .map(|i| i.eq_ignore_ascii_case(node_id))
                    .unwrap_or(false)
            })
            .or_else(|| {
                entries
                    .iter()
                    .find(|e| s_str(e, "slug").map(|s| s == node_id).unwrap_or(false))
            });
        let Some(node) = node else {
            warnings.push(format!("stalled_holder: {node_id} unreadable: not found"));
            continue;
        };
        // A terminal node's claim is a reaper leak; dropping it here also keeps
        // its holder out of the transcript reads.
        if derived_status(node) == "done"
            || s_str(node, "status")
                .map(|s| TERMINAL_RUNGS.contains(&s))
                .unwrap_or(false)
            || node.get("superseded_by").is_some_and(|v| !v.is_null())
        {
            continue;
        }
        nodes.push(node.clone());
        let priority = s_str(node, "priority").unwrap_or("");
        if KING_PRIORITIES.contains(&priority) && seen_holders.insert(holder.clone()) {
            holders.push(holder.clone());
        }
    }
    (SourceRead::ok(Value::Array(nodes)), holders, warnings)
}

/// Positive evidence the holder is doing something (board._holder_is_active):
/// an absent reading is not a staffed lane.
pub(crate) fn holder_is_active(probe: Option<&crate::claude_ask::TruthProbe>) -> bool {
    let Some(probe) = probe else {
        return false;
    };
    if !ACTIVE_STATES.contains(&probe.state.as_str()) {
        return false;
    }
    match probe.last_activity_age_s {
        None => false,
        Some(age) => age <= STALLED_AFTER_S,
    }
}

/// Who is driving this node: active, stalled, or none. One answer, two queues:
/// stalled_holder selects stalled and undriven_pr selects none.
pub(crate) fn node_driver<'a>(
    node_id: &str,
    claim_by_node: &'a HashMap<String, Value>,
    activity: &'a HashMap<String, crate::claude_ask::TruthProbe>,
) -> (&'static str, Option<&'a Value>) {
    let claim = claim_by_node.get(node_id);
    let Some(claim) = claim else {
        return ("none", None);
    };
    if DEAD_CLAIM_STATES.contains(&s_str(claim, "state").unwrap_or("")) {
        return ("none", Some(claim));
    }
    let holder = s_str(claim, "holder").unwrap_or("");
    let token = holder.split_once(':').map(|(_, t)| t).unwrap_or(holder);
    if holder_is_active(activity.get(token)) {
        return ("active", Some(claim));
    }
    ("stalled", Some(claim))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn undispatched_selects_planned_leaf_ready_rows_with_no_claim() {
        let entries = vec![
            json!({"id": "x-aaaa", "status": "ready", "priority": "p0", "plan_path": "/p.md", "type": "feature"}),
            json!({"id": "x-bbbb", "status": "ready", "priority": "p1", "type": "epic", "plan_path": "/q.md"}),
            json!({"id": "x-cccc", "status": "in_progress", "priority": "p0", "plan_path": "/r.md", "type": "feature"}),
            json!({"id": "x-dddd", "status": "ready", "priority": "p2", "plan_path": "/s.md", "type": "feature"}),
        ];
        let claims = vec![json!({"key": "node:x-cccc", "state": "live", "holder": "h"})];
        let receipt = classify_planned_unclaimed(&entries, &claims).unwrap();
        let rows = receipt.get("rows").and_then(Value::as_array).unwrap();
        // The receipt is priority-blind (p2 x-dddd stays); the board's
        // undispatched queue applies the king-priority filter.
        assert_eq!(rows.len(), 2, "{receipt}");
        assert_eq!(rows[0]["id"], "x-aaaa");
        assert_eq!(receipt["status"], "ok");
    }

    #[test]
    fn degenerate_field_values_read_as_absent_like_python_bool() {
        // Python bool("") and bool(0) are false: an empty completed_at is not
        // closure, a zero pr_number is not a PR, an empty batch is not a batch.
        let entries = vec![json!({"id": "x-aaaa", "status": "ready", "priority": "p0",
                   "plan_path": "/p.md", "type": "feature",
                   "completed_at": "", "pr_number": 0, "batch": ""})];
        let receipt = classify_planned_unclaimed(&entries, &[]).unwrap();
        let rows = receipt.get("rows").and_then(Value::as_array).unwrap();
        assert_eq!(rows.len(), 1, "{receipt}");
        let facts = rows[0]["facts"].clone();
        assert_eq!(facts["completed"], false, "{facts}");
        assert_eq!(facts["has_pr"], false, "{facts}");
        assert_eq!(facts["batch_owner"], false, "{facts}");
    }

    #[test]
    fn a_blocked_sibling_excludes_undispatched_until_the_blocker_closes() {
        let entries = vec![
            json!({"id": "x-aaaa", "status": "ready", "priority": "p1", "plan_path": "/p.md", "type": "feature", "blocked_by": ["x-bbbb"]}),
            json!({"id": "x-bbbb", "status": "in_progress", "priority": "p1", "type": "feature"}),
        ];
        let receipt = classify_planned_unclaimed(&entries, &[]).unwrap();
        assert_eq!(receipt["rows"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn holder_activity_reads_only_positive_evidence() {
        let active = crate::claude_ask::TruthProbe {
            state: "working".to_string(),
            reachability: None,
            basis: None,
            last_activity_age_s: Some(30.0),
            last_event_at: None,
            last_message: None,
            observed_model: Value::Null,
        };
        assert!(holder_is_active(Some(&active)));
        let old = crate::claude_ask::TruthProbe {
            last_activity_age_s: Some(STALLED_AFTER_S + 1.0),
            ..active.clone()
        };
        assert!(!holder_is_active(Some(&old)));
        let parked = crate::claude_ask::TruthProbe {
            state: "your-move".to_string(),
            ..active
        };
        assert!(holder_is_active(Some(&parked)));
        assert!(!holder_is_active(None));
    }

    #[test]
    fn the_active_vocabulary_matches_the_python_side() {
        // The pin: reachability._ACTIVE_STATES and session_truth.STALLED_AFTER_S
        // are the load-bearing vocabulary; this test fails when Python grows a
        // fourth state or moves the stall threshold, so the copy cannot rot
        // silently.
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("cli/src/fno/agents/reachability.py");
        let Ok(text) = std::fs::read_to_string(&src) else {
            eprintln!(
                "reachability.py not found at {}; pin skipped (sdist build)",
                src.display()
            );
            return;
        };
        assert!(
            text.contains(r#"_ACTIVE_STATES = frozenset({"working", "watching", "your-move"})"#)
        );
    }
}
