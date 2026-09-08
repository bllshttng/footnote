//! Pure classification over the fetched sources: undispatched selection,
//! claimed-node reads, holder activity, driver state.
use super::prs::derived_status;
use super::{s_str, truthy, SourceRead, DEAD_CLAIM_STATES, KING_PRIORITIES, TERMINAL_RUNGS};
use serde_json::Value;
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
pub(crate) fn holder_is_active(probe: Option<&crate::truth_probe::TruthProbe>) -> bool {
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

/// A node bound to a PR, by `pr_number` or any `additional_prs` entry.
pub(crate) fn node_has_pr(node: &Value) -> bool {
    node.get("pr_number").map(truthy).unwrap_or(false)
        || node
            .get("additional_prs")
            .and_then(Value::as_array)
            .map(|extras| {
                extras
                    .iter()
                    .any(|e| e.is_object() && e.get("number").map(truthy).unwrap_or(false))
            })
            .unwrap_or(false)
}

/// Who is driving this node: active, stalled, crowned, or none. One answer,
/// three queues: stalled_holder selects stalled, undriven_pr and
/// unheld_progress select none. `crowned` is a live crown driving the epic it
/// reigns over: scope ids reach the build only through a king manifest, and
/// the session holding that manifest is the one building, so a scope hit is a
/// live crown. The epic carries no claim of its own (a crown is not a claim),
/// so without this state the reigning epic reads "none" and no verb can clear
/// the row. A PR bound to the epic keeps it reading none - undriven_pr owns
/// that shape, and a PR needs a driver of its own. In-scope leaves stay
/// claim-driven: a dead worker under a crown is still a dead handoff.
pub(crate) fn node_driver<'a>(
    node: &Value,
    claim_by_node: &'a HashMap<String, Value>,
    activity: &'a HashMap<String, crate::truth_probe::TruthProbe>,
    crown_ids: Option<&HashSet<String>>,
) -> (&'static str, Option<&'a Value>) {
    let node_id = s_str(node, "id").unwrap_or("");
    let crowned = crown_ids.is_some_and(|ids| ids.contains(node_id))
        && s_str(node, "type") == Some("epic")
        && !node_has_pr(node);
    let claim = claim_by_node.get(node_id);
    let Some(claim) = claim else {
        if crowned {
            return ("crowned", None);
        }
        return ("none", None);
    };
    if DEAD_CLAIM_STATES.contains(&s_str(claim, "state").unwrap_or("")) {
        if crowned {
            return ("crowned", None);
        }
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
    use serde_json::json;

    #[test]
    fn holder_activity_reads_only_positive_evidence() {
        let active = crate::truth_probe::TruthProbe {
            state: "working".to_string(),
            harness_title: None,
            reachability: None,
            basis: None,
            last_activity_age_s: Some(30.0),
            last_event_at: None,
            last_message: None,
            observed_model: Value::Null,
        };
        assert!(holder_is_active(Some(&active)));
        let old = crate::truth_probe::TruthProbe {
            last_activity_age_s: Some(STALLED_AFTER_S + 1.0),
            ..active.clone()
        };
        assert!(!holder_is_active(Some(&old)));
        let parked = crate::truth_probe::TruthProbe {
            state: "your-move".to_string(),
            ..active
        };
        assert!(holder_is_active(Some(&parked)));
        assert!(!holder_is_active(None));
    }

    #[test]
    fn a_live_crown_drives_the_epic_but_not_its_leaves() {
        let epic = json!({"id": "x-epic", "type": "epic"});
        let leaf = json!({"id": "x-leaf", "parent": "x-epic"});
        let claims: HashMap<String, Value> = HashMap::new();
        let activity = HashMap::new();
        let crown: HashSet<String> = ["x-epic", "x-leaf"]
            .map(str::to_string)
            .into_iter()
            .collect();
        assert_eq!(
            node_driver(&epic, &claims, &activity, Some(&crown)).0,
            "crowned"
        );
        // an epic bound to its own PR keeps reading none: undriven_pr owns
        // that shape, and a PR needs a driver of its own (measured 2026-09-06:
        // six graph epics carry a pr_number)
        let epic_pr = json!({"id": "x-epic", "type": "epic", "pr_number": 42});
        assert_eq!(
            node_driver(&epic_pr, &claims, &activity, Some(&crown)).0,
            "none"
        );
        // without the crown the same epic is an unheld dead handoff
        assert_eq!(node_driver(&epic, &claims, &activity, None).0, "none");
        // an in-scope leaf stays claim-driven
        assert_eq!(
            node_driver(&leaf, &claims, &activity, Some(&crown)).0,
            "none"
        );
        // a live claim outranks the crown
        let mut held = claims.clone();
        held.insert(
            "x-epic".to_string(),
            json!({"key": "node:x-epic", "state": "live", "holder": "claude:h"}),
        );
        assert_eq!(
            node_driver(&epic, &held, &activity, Some(&crown)).0,
            "stalled"
        );
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
