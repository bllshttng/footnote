//! Reconcile's self-heal sweeps: what a merged PR closes, and who shipped
//! it. Ported from graph/_closures.py and the strand sweep half of
//! graph/strand.py (the liveness/cascade halves already live in
//! workflows.rs). Every function mutates a plain entries list in place,
//! inside the store's mutator and under the lock.

use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

use super::merge_evidence::node_pr_refs;
use super::supersession::normalize_surface;
use crate::backlog::workflows::{
    apply_completion_fields, auto_closed_note, children_all_closed, is_live, pr_ref_set,
    release_contained_row, reopen_outranks_child_closes, reopen_outranks_merge,
    reparent_live_children, text_at,
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

/// The merge evidence a cascade judge reads: the owner PR's changed-file
/// set and the PR number (a contained child the PR body bound carries this
/// number).
pub(crate) struct CascadeEvidence<'a> {
    pub(crate) changed_files: &'a [String],
    pub(crate) pr_number: i64,
}

/// What one cascade pass did: the closed ids and the released ids.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct ContainedCascade {
    pub(crate) closed: Vec<String>,
    pub(crate) released: Vec<String>,
}

/// Close every node that shipped inside `node_id`'s PR. With `merged_at`
/// the child reopen keys on the merge, not the owner's completed_at.
pub(crate) fn cascade_close_contained(
    entries: &mut [Value],
    node_id: &str,
    merged_at: Option<&str>,
    evidence: Option<&CascadeEvidence>,
) -> ContainedCascade {
    let unit_index = entries
        .iter()
        .position(|e| text_at(e, "id") == Some(node_id));
    // Cloned, not borrowed: the mutation loop below needs `entries` mutably,
    // and the guard reads the unit row only through this clone.
    let unit: Option<Value> = unit_index.map(|i| entries[i].clone());
    let pr = unit
        .as_ref()
        .and_then(|u| u.get("pr_number"))
        .and_then(Value::as_i64);
    let where_word = pr
        .map(|p| format!("PR #{p}"))
        .unwrap_or_else(|| "its PR".into());
    let note = format!(
        "auto-closed: shipped inside {node_id} ({where_word}); cost and session are recorded on {node_id}"
    );

    let changed: BTreeSet<String> = evidence
        .map(|ev| {
            ev.changed_files
                .iter()
                .filter(|p| !p.trim().is_empty())
                .map(|p| normalize_surface(p))
                .collect()
        })
        .unwrap_or_default();
    let owner_refs = pr_ref_set(entries, node_id);
    let mut closed: Vec<String> = Vec::new();
    let mut released: Vec<String> = Vec::new();
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
        } else if let Some(unit) = &unit {
            if reopen_outranks_child_closes(e, &[unit]) {
                continue;
            }
        }
        let Some(nid) = text_at(e, "id").map(str::to_string) else {
            continue; // unidentifiable row: nothing to report, nothing to close
        };
        // A child that declared surfaces closes only on evidence its work
        // rode this PR: a changed-file match, or the PR body binding the
        // child to this PR. Without it the containment releases (the row
        // keeps its parent, re-dispatches, and carries `released_from`).
        let declared: Vec<String> = e
            .get("containment_surfaces")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .map(normalize_surface)
                    .collect()
            })
            .unwrap_or_default();
        if !declared.is_empty() {
            let body_bound = evidence
                .map(|ev| {
                    ev.pr_number > 0
                        && e.get("pr_number").and_then(Value::as_i64) == Some(ev.pr_number)
                })
                .unwrap_or(false);
            let matched = declared.iter().any(|s| changed.contains(s));
            if !matched && !body_bound {
                release_contained_row(e, node_id, &owner_refs);
                released.push(nid);
                continue;
            }
        }
        // merged_at is set only when reconcile resolved MERGED from gh, so
        // the child inherits the stamp instead of reading merge_status null
        // (a null made the merge reaper hold the request the node shipped in).
        apply_completion_fields(e, merged_at.is_some());
        e.as_object_mut()
            .expect("row is an object")
            .insert("completion_note".into(), json!(note));
        closed.push(nid);
    }
    ContainedCascade { closed, released }
}

/// Open nodes whose delivery unit is ALREADY done - closeable right now.
/// A child that declared surfaces is named here too: the sweep's cascade
/// runs with no file evidence, and the gate then releases it instead of
/// closing (see `cascade_close_contained`).
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

/// Close (or release) every node `strandable_contained_ids` names, grouped
/// by owner so each node gets the same note the merge-time cascade writes.
/// The cascade runs with no file evidence: a declared child releases, an
/// undeclared child closes, and the reopen guard holds a deliberate reopen.
pub(crate) fn sweep_close_stranded_contained(entries: &mut [Value]) -> ContainedCascade {
    let mut out = ContainedCascade::default();
    let stranded = strandable_contained_ids(entries);
    let mut owners: BTreeSet<String> = BTreeSet::new();
    for e in entries.iter() {
        let Some(nid) = text_at(e, "id") else {
            continue;
        };
        if stranded.contains(nid) {
            if let Some(owner) = text_at(e, "contained_in") {
                owners.insert(owner.to_string());
            }
        }
    }
    for owner in owners {
        let r = cascade_close_contained(entries, &owner, None, None);
        out.closed.extend(r.closed);
        out.released.extend(r.released);
    }
    out
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
            json!({"id": "x-cccc", "parent": "x-epic", "status": "done", "completed_at": "2026-10-01T00:00:00Z"}),
        ];
        let ids = strandable_epic_ids(&entries);
        assert_eq!(ids, BTreeSet::from(["x-epic".to_string()]));
    }

    #[test]
    fn a_live_child_keeps_its_epic_open() {
        let entries = vec![
            seed("x-epic", None as Option<&str>),
            json!({"id": "x-cccc", "parent": "x-epic"}),
        ];
        assert!(strandable_epic_ids(&entries).is_empty());
    }

    #[test]
    fn the_sweep_closes_ready_epics_to_a_fixpoint() {
        let mut entries = vec![
            seed("x-mission", None as Option<&str>),
            seed("x-epic", Some("x-mission")),
            json!({"id": "x-cccc", "parent": "x-epic", "status": "done", "completed_at": "2026-10-01T00:00:00Z"}),
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
        let closed =
            cascade_close_contained(&mut entries, "x-owner", Some("2026-10-01T00:00:00Z"), None);
        assert_eq!(closed.closed, vec!["x-carried".to_string()]);
        assert!(closed.released.is_empty());
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
        let closed =
            cascade_close_contained(&mut entries, "x-owner", Some("2026-10-01T00:00:00Z"), None);
        assert!(closed.closed.is_empty());
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
        assert_eq!(closed.closed, vec!["x-carried".to_string()]);
        assert!(entries[1]["completion_note"]
            .as_str()
            .unwrap_or_default()
            .contains("shipped inside x-owner"));
    }

    #[test]
    fn a_surface_match_closes_a_declared_child() {
        let mut entries = vec![
            json!({"id": "x-owner", "pr_number": 9}),
            json!({
                "id": "x-carried",
                "contained_in": "x-owner",
                "containment_surfaces": ["scripts/ci/check-file-budget.sh"]
            }),
        ];
        let files = vec!["scripts/ci/check-file-budget.sh".to_string()];
        let ev = CascadeEvidence {
            changed_files: &files,
            pr_number: 9,
        };
        let out = cascade_close_contained(
            &mut entries,
            "x-owner",
            Some("2026-10-01T00:00:00Z"),
            Some(&ev),
        );
        assert_eq!(out.closed, vec!["x-carried".to_string()]);
        assert!(out.released.is_empty());
    }

    #[test]
    fn a_no_match_releases_the_containment() {
        let mut entries = vec![
            json!({"id": "x-owner", "pr_number": 9}),
            json!({
                "id": "x-carried",
                "contained_in": "x-owner",
                "containment_surfaces": ["scripts/ci/check-file-budget.sh"]
            }),
        ];
        let files = vec!["crates/fno/src/theme.rs".to_string()];
        let ev = CascadeEvidence {
            changed_files: &files,
            pr_number: 9,
        };
        let out = cascade_close_contained(
            &mut entries,
            "x-owner",
            Some("2026-10-01T00:00:00Z"),
            Some(&ev),
        );
        assert_eq!(out.released, vec!["x-carried".to_string()]);
        assert!(out.closed.is_empty());
        assert!(entries[1].get("completed_at").is_none());
        assert!(entries[1].get("contained_in").is_none());
        assert_eq!(entries[1]["released_from"], "x-owner");
    }

    #[test]
    fn a_body_bound_child_closes_even_without_a_file_match() {
        let mut entries = vec![
            json!({"id": "x-owner", "pr_number": 9}),
            json!({
                "id": "x-carried",
                "contained_in": "x-owner",
                "pr_number": 9,
                "containment_surfaces": ["scripts/ci/check-file-budget.sh"]
            }),
        ];
        let files = vec!["crates/fno/src/theme.rs".to_string()];
        let ev = CascadeEvidence {
            changed_files: &files,
            pr_number: 9,
        };
        let out = cascade_close_contained(
            &mut entries,
            "x-owner",
            Some("2026-10-01T00:00:00Z"),
            Some(&ev),
        );
        assert_eq!(out.closed, vec!["x-carried".to_string()]);
    }

    #[test]
    fn a_declared_child_of_a_done_owner_releases_instead_of_stranding() {
        let mut entries = vec![
            json!({"id": "x-owner", "status": "done", "completed_at": "2026-10-01T00:00:00Z"}),
            json!({
                "id": "x-carried",
                "contained_in": "x-owner",
                "containment_surfaces": ["scripts/ci/check-file-budget.sh"]
            }),
        ];
        let out = sweep_close_stranded_contained(&mut entries);
        assert_eq!(out.released, vec!["x-carried".to_string()]);
        assert!(out.closed.is_empty());
        assert!(entries[1].get("contained_in").is_none());
        assert_eq!(entries[1]["released_from"], "x-owner");
        assert!(entries[1].get("completed_at").is_none());
    }

    #[test]
    fn an_orphan_under_a_terminal_parent_is_strandable() {
        // argv-fence: exempt - graph fixture ids in a Rust test literal, not a worker seed.
        let entries = vec![done("x-dddd"), seed("x-cccc", Some("x-dddd"))];
        assert_eq!(
            strandable_orphan_ids(&entries),
            BTreeSet::from(["x-cccc".to_string()])
        );
    }

    #[test]
    fn the_orphan_sweep_moves_kids_to_the_nearest_live_ancestor() {
        let mut entries = vec![
            seed("x-eeee", None as Option<&str>),
            json!({"id": "x-dddd", "parent": "x-eeee", "status": "done", "completed_at": "2026-10-01T00:00:00Z"}),
            seed("x-cccc", Some("x-dddd")),
        ];
        let moved = sweep_reparent_stranded_orphans(&mut entries);
        assert_eq!(
            moved,
            vec![("x-cccc".to_string(), Some("x-eeee".to_string()))]
        );
        assert_eq!(entries[2]["parent"], "x-eeee");
        assert!(sweep_reparent_stranded_orphans(&mut entries).is_empty());
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
