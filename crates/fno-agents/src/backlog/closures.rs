//! Reconcile's self-heal sweeps: what a merged PR closes, and who shipped
//! it. Ported from graph/_closures.py and the strand sweep half of
//! graph/strand.py (the liveness/cascade halves already live in
//! workflows.rs). Every function mutates a plain entries list in place,
//! inside the store's mutator and under the lock.

use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};

use super::merge_evidence::node_pr_refs;
use crate::backlog::workflows::{
    apply_completion_fields, auto_closed_note, children_all_closed, is_live,
    reopen_outranks_child_closes, reopen_outranks_merge, reparent_live_children, text_at,
};

/// Open epics (parents) that pass `children_all_closed` - closeable now.
pub(crate) fn strandable_epic_ids(entries: &[Value]) -> BTreeSet<String> {
    let mut children_by_parent: BTreeMap<&str, Vec<&Value>> = BTreeMap::new();
    for e in entries {
        if let Some(pid) = text_at(e, "parent") {
            children_by_parent.entry(pid).or_default().push(e);
        }
    }
    let mut out = BTreeSet::new();
    for (pid, kids) in children_by_parent {
        let Some(parent) = entries.iter().find(|e| text_at(e, "id") == Some(pid)) else {
            continue;
        };
        if parent
            .get("completed_at")
            .map(|v| !v.is_null())
            .unwrap_or(false)
        {
            continue;
        }
        if children_all_closed(parent, &kids) && !reopen_outranks_child_closes(parent, &kids) {
            out.insert(pid.to_string());
        }
    }
    out
}

/// Close every open epic whose children are all done (self-heal/migration).
pub(crate) fn sweep_close_done_epics(entries: &mut [Value]) -> Vec<String> {
    let mut closed: Vec<String> = Vec::new();
    for _ in 0..64 {
        // fixpoint, depth-capped against a malformed cycle
        let ready = strandable_epic_ids(entries);
        if ready.is_empty() {
            break;
        }
        for pid in ready {
            let Some(index) = entries
                .iter()
                .position(|e| text_at(e, "id") == Some(pid.as_str()))
            else {
                continue;
            };
            let parent = &mut entries[index];
            if parent
                .get("completed_at")
                .map(|v| !v.is_null())
                .unwrap_or(false)
            {
                continue;
            }
            apply_completion_fields(parent, false);
            if text_at(parent, "completion_note").is_none() {
                let note = auto_closed_note(parent);
                parent
                    .as_object_mut()
                    .expect("row is an object")
                    .insert("completion_note".into(), json!(note));
            }
            closed.push(pid);
        }
    }
    closed
}

/// Give a node carried inside another node's PR the `do` rows that shipped
/// it. Every writer of `sessions[]` is keyed to a session that OWNS the
/// node, so a worker that claims one node and ships several leaves its
/// passengers with a merged PR and no session at all. Only `do` travels
/// (phase execute): blueprint and ship happened to the owner's node. The
/// key is the pr_url, because five repos share this graph and their PR
/// numbers interleave; a link with no url is skipped.
pub(crate) fn sweep_stamp_carried_sessions(entries: &mut [Value]) -> Vec<String> {
    let mut donors: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for e in entries.iter() {
        let do_rows: Vec<Value> = e
            .get("sessions")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter(|r| r.get("phase").and_then(Value::as_str) == Some("execute"))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        if do_rows.is_empty() {
            continue;
        }
        for (_n, url) in node_pr_refs(e) {
            if let Some(url) = url.filter(|u| !u.is_empty()) {
                donors
                    .entry(url)
                    .or_default()
                    .extend(do_rows.iter().cloned());
            }
        }
    }

    let mut stamped: Vec<String> = Vec::new();
    for e in entries.iter_mut() {
        if e.get("sessions").map(|s| !s.is_null()).unwrap_or(false) {
            continue;
        }
        let Some(nid) = text_at(e, "id").map(str::to_string) else {
            continue;
        };
        // Two nodes must not share a row, nor the nested dicts inside it:
        // clone per node.
        let mut carried: BTreeMap<(String, String, String), Value> = BTreeMap::new();
        for (_n, url) in node_pr_refs(e) {
            let Some(url) = url.filter(|u| !u.is_empty()) else {
                continue;
            };
            for row in donors.get(&url).into_iter().flatten() {
                let key = (
                    text_at(row, "phase").unwrap_or("").to_string(),
                    text_at(row, "harness").unwrap_or("").to_string(),
                    text_at(row, "session_id").unwrap_or("").to_string(),
                );
                carried.entry(key).or_insert_with(|| row.clone());
            }
        }
        if !carried.is_empty() {
            let rows: Vec<Value> = carried.into_values().collect();
            e.as_object_mut()
                .expect("row is an object")
                .insert("sessions".into(), Value::Array(rows));
            stamped.push(nid);
        }
    }
    stamped
}

/// Close every node that shipped inside `node_id`'s PR. With `merged_at`
/// the child reopen keys on the merge, not the owner's completed_at.
pub(crate) fn cascade_close_contained(
    entries: &mut [Value],
    node_id: &str,
    merged_at: Option<&str>,
) -> Vec<String> {
    let unit_index = entries
        .iter()
        .position(|e| text_at(e, "id") == Some(node_id));
    let unit = unit_index.map(|i| &entries[i]);
    let pr = unit
        .and_then(|u| u.get("pr_number"))
        .and_then(Value::as_i64);
    let where_word = pr
        .map(|p| format!("PR #{p}"))
        .unwrap_or_else(|| "its PR".into());
    let note = format!(
        "auto-closed: shipped inside {node_id} ({where_word}); cost and session are recorded on {node_id}"
    );

    let mut closed: Vec<String> = Vec::new();
    for e in entries.iter_mut() {
        if text_at(e, "contained_in") != Some(node_id) {
            continue;
        }
        if e.get("completed_at").map(|v| !v.is_null()).unwrap_or(false) {
            continue; // already closed (out of band, or a previous sweep)
        }
        // A reopen postdating the close evidence holds.
        if let Some(merged_at) = merged_at {
            if reopen_outranks_merge(e, merged_at) {
                continue;
            }
        } else if let Some(unit) = unit {
            if reopen_outranks_child_closes(e, std::slice::from_ref(&unit)) {
                continue;
            }
        }
        let Some(nid) = text_at(e, "id").map(str::to_string) else {
            continue; // unidentifiable row: nothing to report, nothing to close
        };
        // merged_at is set only when reconcile resolved MERGED from gh, so
        // the child inherits the stamp instead of reading merge_status null
        // (a null made the merge reaper hold the request the node shipped in).
        apply_completion_fields(e, merged_at.is_some());
        e.as_object_mut()
            .expect("row is an object")
            .insert("completion_note".into(), json!(note));
        closed.push(nid);
    }
    closed
}

/// Open nodes whose delivery unit is ALREADY done - closeable right now.
pub(crate) fn strandable_contained_ids(entries: &[Value]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for e in entries {
        if e.get("completed_at").map(|v| !v.is_null()).unwrap_or(false) {
            continue;
        }
        let Some(owner_id) = text_at(e, "contained_in") else {
            continue;
        };
        let Some(owner) = entries.iter().find(|r| text_at(r, "id") == Some(owner_id)) else {
            continue;
        };
        let owner_done = owner
            .get("completed_at")
            .map(|v| !v.is_null())
            .unwrap_or(false);
        let Some(nid) = text_at(e, "id") else {
            continue;
        };
        if owner_done && !reopen_outranks_child_closes(e, std::slice::from_ref(&owner)) {
            out.insert(nid.to_string());
        }
    }
    out
}

/// Close every node `strandable_contained_ids` names, grouped by owner so
/// each node gets the same note the merge-time cascade writes.
pub(crate) fn sweep_close_stranded_contained(entries: &mut [Value]) -> Vec<String> {
    let stranded = strandable_contained_ids(entries);
    if stranded.is_empty() {
        return Vec::new();
    }
    let mut owners: BTreeSet<String> = BTreeSet::new();
    for e in entries.iter() {
        if let Some(nid) = text_at(e, "id") {
            if stranded.contains(nid) {
                if let Some(owner) = text_at(e, "contained_in") {
                    owners.insert(owner.to_string());
                }
            }
        }
    }
    let mut closed: Vec<String> = Vec::new();
    for owner in owners {
        closed.extend(cascade_close_contained(entries, &owner, None));
    }
    closed
}

/// Live non-contained node ids whose DIRECT parent exists and is terminal.
/// Read-only detector, the parent-axis twin of strandable_contained_ids.
pub(crate) fn strandable_orphan_ids(entries: &[Value]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for e in entries {
        if !is_live(e) || text_at(e, "contained_in").is_some() {
            continue;
        }
        let Some(pid) = text_at(e, "parent") else {
            continue;
        };
        let Some(parent) = entries.iter().find(|r| text_at(r, "id") == Some(pid)) else {
            continue;
        };
        let Some(nid) = text_at(e, "id") else {
            continue;
        };
        if !is_live(parent) {
            out.insert(nid.to_string());
        }
    }
    out
}

/// Re-parent every stranded child on the board; one pass, no fixpoint.
pub(crate) fn sweep_reparent_stranded_orphans(
    entries: &mut [Value],
) -> Vec<(String, Option<String>)> {
    let stranded = strandable_orphan_ids(entries);
    if stranded.is_empty() {
        return Vec::new();
    }
    let mut parents: BTreeSet<String> = BTreeSet::new();
    for e in entries.iter() {
        let Some(nid) = text_at(e, "id") else {
            continue;
        };
        if !stranded.contains(nid) {
            continue;
        }
        if let Some(pid) = text_at(e, "parent") {
            parents.insert(pid.to_string());
        }
    }
    let mut moved: Vec<(String, Option<String>)> = Vec::new();
    for pid in parents {
        moved.extend(reparent_live_children(entries, &pid));
    }
    moved
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn seed(id: &str, parent: Option<&str>) -> Value {
        let mut row = json!({"id": id});
        if let Some(p) = parent {
            row["parent"] = json!(p);
        }
        row
    }

    fn done(id: &str) -> Value {
        json!({"id": id, "status": "done", "completed_at": "2026-10-01T00:00:00Z"})
    }

    #[test]
    fn an_epic_whose_children_all_closed_is_strandable() {
        let entries = vec![
            seed("x-epic", None as Option<&str>),
            json!({"id": "x-kid", "parent": "x-epic", "status": "done", "completed_at": "2026-10-01T00:00:00Z"}),
        ];
        let ids = strandable_epic_ids(&entries);
        assert_eq!(ids, BTreeSet::from(["x-epic".to_string()]));
    }

    #[test]
    fn a_live_child_keeps_its_epic_open() {
        let entries = vec![
            seed("x-epic", None as Option<&str>),
            json!({"id": "x-kid", "parent": "x-epic"}),
        ];
        assert!(strandable_epic_ids(&entries).is_empty());
    }

    #[test]
    fn the_sweep_closes_ready_epics_to_a_fixpoint() {
        let mut entries = vec![
            seed("x-mission", None as Option<&str>),
            seed("x-epic", Some("x-mission")),
            done("x-kid"),
        ];
        let closed = sweep_close_done_epics(&mut entries);
        assert_eq!(closed, vec!["x-epic".to_string(), "x-mission".to_string()]);
        assert_eq!(entries[0]["status"], "done");
        let note = entries[0]["completion_note"].as_str().unwrap_or_default();
        assert!(note.contains("all children complete"), "{note}");
    }

    #[test]
    fn contained_children_close_with_their_owner() {
        let mut entries = vec![
            json!({"id": "x-owner", "pr_number": 9}),
            json!({"id": "x-carried", "contained_in": "x-owner"}),
        ];
        let closed = cascade_close_contained(&mut entries, "x-owner", Some("2026-10-01T00:00:00Z"));
        assert_eq!(closed, vec!["x-carried".to_string()]);
        assert_eq!(entries[1]["status"], "done");
        assert_eq!(entries[1]["merge_status"], "merged");
        assert!(entries[1]["completion_note"]
            .as_str()
            .unwrap_or_default()
            .contains("shipped inside x-owner"));
    }

    #[test]
    fn a_child_reopen_postdating_the_merge_holds_the_close() {
        let mut entries = vec![
            json!({"id": "x-owner", "pr_number": 9}),
            json!({
                "id": "x-carried",
                "contained_in": "x-owner",
                "reopened_at": "2026-10-02T00:00:00Z",
                "reopened_reason": "still needed",
            }),
        ];
        let closed = cascade_close_contained(&mut entries, "x-owner", Some("2026-10-01T00:00:00Z"));
        assert!(closed.is_empty());
        assert!(entries[1].get("completed_at").is_none());
    }

    #[test]
    fn a_contained_node_whose_owner_already_closed_is_strandable() {
        let entries = vec![
            done("x-owner"),
            json!({"id": "x-carried", "contained_in": "x-owner"}),
        ];
        let ids = strandable_contained_ids(&entries);
        assert_eq!(ids, BTreeSet::from(["x-carried".to_string()]));
    }

    #[test]
    fn the_stranded_contained_sweep_names_its_unit() {
        let mut entries = vec![
            json!({"id": "x-owner", "pr_number": 9, "status": "done", "completed_at": "2026-10-01T00:00:00Z"}),
            json!({"id": "x-carried", "contained_in": "x-owner"}),
        ];
        let closed = sweep_close_stranded_contained(&mut entries);
        assert_eq!(closed, vec!["x-carried".to_string()]);
        assert!(entries[1]["completion_note"]
            .as_str()
            .unwrap_or_default()
            .contains("shipped inside x-owner"));
    }

    #[test]
    fn an_orphan_under_a_terminal_parent_is_strandable() {
        let entries = vec![done("x-dead"), seed("x-kid", Some("x-dead"))];
        assert_eq!(
            strandable_orphan_ids(&entries),
            BTreeSet::from(["x-kid".to_string()])
        );
    }

    #[test]
    fn the_orphan_sweep_moves_kids_to_the_nearest_live_ancestor() {
        let mut entries = vec![
            done("x-dead"),
            seed("x-kid", Some("x-dead")),
            seed("x-live", Some("x-dead")),
        ];
        let moved = sweep_reparent_stranded_orphans(&mut entries);
        assert_eq!(
            moved,
            vec![("x-kid".to_string(), Some("x-live".to_string()))]
        );
        assert_eq!(entries[1]["parent"], "x-live");
    }

    #[test]
    fn carried_sessions_travel_on_a_shared_pr_url() {
        let mut entries = vec![
            json!({
                "id": "x-owner",
                "pr_number": 5,
                "pr_url": "https://github.com/o/r/pull/5",
                "sessions": [{"phase": "execute", "harness": "claude", "session_id": "s1"}],
            }),
            json!({
                "id": "x-carried",
                "pr_number": 5,
                "pr_url": "https://github.com/o/r/pull/5",
            }),
        ];
        let stamped = sweep_stamp_carried_sessions(&mut entries);
        assert_eq!(stamped, vec!["x-carried".to_string()]);
        assert_eq!(entries[1]["sessions"][0]["session_id"], "s1");
        // A node with its own sessions never gets stamped.
        assert!(entries[0]["sessions"][0]["session_id"] == "s1");
    }
}
