//! The epic child cap. `config.backlog.epic_max_open_children`, read from
//! the `config.toml` beside the graph, bounds the open direct children one
//! epic may hold: a write that parents one more open child under a full
//! epic is refused, so new findings start a new small epic instead of
//! growing one that never finishes. The check runs at both publication
//! seams, so every door (update --parent, idea --parent, contain,
//! decompose, the rollup auto-link, api node_create) meets the same
//! refusal. Unset means no cap, which is the OSS default.

use serde_json::Value;
use std::path::Path;

/// `config.backlog.epic_max_open_children`, beside the graph. `None` = no
/// cap. Only positive integers count: a missing file, `0`, `-1` and a
/// quoted `"15"` all read as no cap.
pub fn configured_cap(graph: &Path) -> Option<usize> {
    let dir = graph.parent().unwrap_or_else(|| Path::new(""));
    crate::backlog_ready::backlog_config_int(dir, "epic_max_open_children")
        .filter(|n| *n > 0)
        .map(|n| n as usize)
}

/// A row holds its parent open: not terminal and not deferred (a lead does
/// not work parked rows, so a wont-do child never holds its epic open).
fn row_is_open(row: &Value) -> bool {
    !crate::graph_store::is_terminal_entry(row)
        && row.get("status").and_then(Value::as_str) != Some("deferred")
}

fn parent_of(row: &Value) -> Option<&str> {
    row.get("parent")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
}

/// Refuse a write that leaves an epic it grew with more open children than
/// the cap. `pre` is the begin snapshot, `post` the candidate state. With
/// no cap configured this returns `Ok` at once.
pub fn enforce(pre: &[Value], post: &[Value], cap: Option<usize>) -> Result<(), String> {
    let Some(cap) = cap else {
        return Ok(());
    };
    // Borrowed id indexes, not a linear scan per row: every graph write
    // reaches this seam, and a `find()` inside the row loop is quadratic
    // over the whole graph.
    let post_by_id = crate::graph_store::index_by_id(post);
    let pre_by_id = crate::graph_store::index_by_id(pre);
    // Rows this write newly parents under an epic: open, parent non-empty
    // and not self, parent typed epic, and the edge is new (the row was
    // absent, or its parent before the write differed).
    let mut grown: Vec<(&str, &Value, Vec<&str>)> = Vec::new();
    for row in post.iter() {
        let (Some(id), true) = (crate::graph_store::entry_id(row), row.is_object()) else {
            continue;
        };
        if !row_is_open(row) {
            continue;
        }
        let Some(epic) = parent_of(row).filter(|p| *p != id) else {
            continue;
        };
        let Some(epic_row) = post_by_id.get(epic).copied() else {
            continue;
        };
        if epic_row.get("type").and_then(Value::as_str) != Some("epic") {
            continue;
        }
        let pre_row = pre_by_id.get(id).copied();
        // The edge is unchanged AND the row was already open, so this write
        // adds no open child. A child coming back from closed (undefer,
        // reopen) does add one, even under an unchanged edge, so it is
        // judged like a fresh edge rather than skipped here.
        // A type update can turn a populated feature into an epic. Its
        // children keep both their edge and their status, so judging the
        // edge alone skips every one of them and the epic is born over
        // cap. The parent must ALSO have been an epic before the write.
        let was_epic = pre_by_id
            .get(epic)
            .map(|r| r.get("type").and_then(Value::as_str) == Some("epic"))
            .unwrap_or(false);
        let already_counted = was_epic
            && pre_row
                .map(|r| parent_of(r) == Some(epic) && row_is_open(r))
                .unwrap_or(false);
        if already_counted {
            continue;
        }
        // Hand-up exemption: the old parent is terminal AFTER the write, so
        // this edge moves existing work (the done/maintain hand-up) and
        // adds none. It covers an ALREADY-OPEN row only, for the same
        // reason the unchanged edge above does: a row coming back from
        // closed is new open work wherever it lands.
        if let Some(old) = pre_row.filter(|r| row_is_open(r)).and_then(parent_of) {
            if post_by_id
                .get(old)
                .copied()
                .map(crate::graph_store::is_terminal_entry)
                .unwrap_or(false)
            {
                continue;
            }
        }
        let entry = match grown.iter_mut().find(|(e, _, _)| *e == epic) {
            Some(entry) => entry,
            None => {
                grown.push((epic, epic_row, Vec::new()));
                grown.last_mut().unwrap()
            }
        };
        entry.2.push(id);
    }
    for (epic, epic_row, children) in grown {
        let open = post
            .iter()
            .filter(|r| row_is_open(r) && parent_of(r) == Some(epic))
            .count();
        if open <= cap {
            continue;
        }
        let title = epic_row
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let title: String = title.chars().take(80).collect();
        let kids = children
            .iter()
            .map(|id| format!("'{}'", id))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "epic cap: refusing to add {kids} under epic '{epic}' ('{title}'): it would hold \
             {open} open children and the cap is {cap} (config backlog.epic_max_open_children). \
             An epic stays small enough to finish, so new work starts a new epic. Next: \
             fno backlog idea \"EPIC: <theme>\" --type epic --difficulty <low|medium|high>, \
             then point this write at the new epic id. If a king leads '{epic}', it adds the \
             new epic to its own crown: fno agents crown <its handle> --scope <each epic it \
             holds> --scope '<new-epic-id>'. That works for an epic the king's own session \
             created. Any other epic needs an attended shell or a crown that contains both."
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn epic(id: &str) -> Value {
        json!({"id": id, "slug": id, "title": "the full epic", "type": "epic",
               "status": "in_progress", "priority": "p1", "domain": "code"})
    }

    fn child(id: &str, parent: &str) -> Value {
        json!({"id": id, "slug": id, "title": id, "type": "feature",
               "status": "idea", "priority": "p2", "domain": "code", "parent": parent})
    }

    fn full_epic_rows() -> Vec<Value> {
        let mut rows = vec![epic("e-1")];
        for i in 1..=15 {
            rows.push(child(&format!("c-{i:02}"), "e-1"));
        }
        rows
    }

    #[test]
    fn a_sixteenth_open_child_is_refused_and_names_the_cap() {
        // AC1-HP, first half: the message names the epic, both counts, the
        // config key and the new-epic verb.
        let pre = full_epic_rows();
        let mut post = pre.clone();
        post.push(child("c-16", "e-1"));
        let error = enforce(&pre, &post, Some(15)).unwrap_err();
        assert!(error.contains("epic cap: refusing to add"), "{error}");
        assert!(error.contains("'c-16'"), "{error}");
        assert!(error.contains("'e-1'"), "{error}");
        assert!(error.contains("16 open children"), "{error}");
        assert!(error.contains("the cap is 15"), "{error}");
        assert!(error.contains("backlog.epic_max_open_children"), "{error}");
        assert!(error.contains("--type epic"), "{error}");
    }

    #[test]
    fn the_same_child_under_a_fresh_epic_passes() {
        // AC1-HP, second half.
        let pre = full_epic_rows();
        let mut post = pre.clone();
        post.push(child("c-16", "e-2"));
        post.push(epic("e-2"));
        enforce(&pre, &post, Some(15)).unwrap();
    }

    #[test]
    fn no_cap_configured_accepts_anything() {
        let pre = full_epic_rows();
        let mut post = pre.clone();
        for i in 16..=40 {
            post.push(child(&format!("c-{i}"), "e-1"));
        }
        enforce(&pre, &post, None).unwrap();
    }

    #[test]
    fn a_hand_up_from_a_terminal_parent_is_exempt() {
        // The parent closes and its live children move in the same write:
        // existing work moves, none is added.
        let mut pre = full_epic_rows();
        let mut post = full_epic_rows();
        post.push(child("c-16", "e-1"));
        // c-16's OLD parent p-old is done after this write.
        pre.push(
            json!({"id": "c-16", "slug": "c-16", "title": "c-16", "type": "feature",
                        "status": "idea", "priority": "p2", "domain": "code",
                        "parent": "p-old"}),
        );
        post.push(
            json!({"id": "p-old", "slug": "p-old", "title": "p-old", "type": "feature",
                         "status": "done", "priority": "p2", "domain": "code"}),
        );
        enforce(&pre, &post, Some(15)).unwrap();
    }

    #[test]
    fn a_closed_row_handed_up_into_a_full_epic_is_still_refused() {
        // The hand-up exemption carries EXISTING OPEN work off a closing
        // parent. A deferred row that lands open under a full epic in the
        // same write is new open work, so the exemption must not cover it.
        let mut pre = full_epic_rows();
        pre.push(json!({"id": "c-16", "slug": "c-16", "title": "c-16",
                        "type": "feature", "status": "deferred", "priority": "p2",
                        "domain": "code", "parent": "p-old"}));
        let mut post = full_epic_rows();
        post.push(child("c-16", "e-1"));
        post.push(json!({"id": "p-old", "slug": "p-old", "title": "p-old",
                         "type": "feature", "status": "done", "priority": "p2",
                         "domain": "code"}));
        let error = enforce(&pre, &post, Some(15)).unwrap_err();
        assert!(error.contains("'c-16'"), "{error}");
        assert!(error.contains("16 open children"), "{error}");
    }

    #[test]
    fn a_status_edit_on_a_child_of_an_over_cap_epic_passes() {
        // The edge is not new, so the write adds no child even though the
        // epic is already over the cap.
        let pre = full_epic_rows();
        let mut post = full_epic_rows();
        for row in post.iter_mut() {
            if crate::graph_store::entry_id(row) == Some("c-01") {
                row.as_object_mut()
                    .unwrap()
                    .insert("status".into(), json!("ready"));
            }
        }
        enforce(&pre, &post, Some(15)).unwrap();
    }

    #[test]
    fn a_closed_child_never_grows_the_epic() {
        for status in ["done", "superseded", "deferred"] {
            let pre = full_epic_rows();
            let mut post = pre.clone();
            post.push(json!({"id": "c-16", "slug": "c-16", "title": "c-16",
                             "type": "feature", "status": status, "priority": "p2",
                             "domain": "code", "parent": "e-1"}));
            enforce(&pre, &post, Some(15)).unwrap();
        }
    }

    #[test]
    fn a_populated_feature_turned_into_an_epic_counts_every_child() {
        // A type update is the other way an epic grows without any edge
        // moving. The children keep their parent and their status, so
        // judging the edge alone lets the epic be BORN over cap.
        let mut pre = full_epic_rows();
        pre[0]
            .as_object_mut()
            .unwrap()
            .insert("type".into(), json!("feature"));
        pre.push(child("c-16", "e-1"));
        let mut post = pre.clone();
        post[0]
            .as_object_mut()
            .unwrap()
            .insert("type".into(), json!("epic"));
        let error = enforce(&pre, &post, Some(15)).unwrap_err();
        assert!(error.contains("16 open children"), "{error}");
        // The control: the same conversion lands while it has room.
        let mut pre = full_epic_rows();
        pre.truncate(15);
        pre[0]
            .as_object_mut()
            .unwrap()
            .insert("type".into(), json!("feature"));
        let mut post = pre.clone();
        post[0]
            .as_object_mut()
            .unwrap()
            .insert("type".into(), json!("epic"));
        enforce(&pre, &post, Some(15)).unwrap();
    }

    #[test]
    fn a_child_coming_back_from_closed_counts_as_growth() {
        // An undefer or reopen raises the open count under an UNCHANGED
        // parent edge. Skipping every unchanged edge let that bypass the
        // cap silently, so a closed-to-open transition is judged too.
        for closed in ["deferred", "done", "superseded"] {
            let mut pre = full_epic_rows();
            pre.push(json!({"id": "c-16", "slug": "c-16", "title": "c-16",
                            "type": "feature", "status": closed, "priority": "p2",
                            "domain": "code", "parent": "e-1"}));
            let mut post = pre.clone();
            for row in post.iter_mut() {
                if crate::graph_store::entry_id(row) == Some("c-16") {
                    let obj = row.as_object_mut().unwrap();
                    obj.insert("status".into(), json!("ready"));
                    obj.insert("superseded_by".into(), Value::Null);
                    obj.insert("completed_at".into(), Value::Null);
                }
            }
            let error = enforce(&pre, &post, Some(15)).unwrap_err();
            assert!(error.contains("'c-16'"), "{closed}: {error}");
            assert!(error.contains("16 open children"), "{closed}: {error}");
        }
    }

    #[test]
    fn a_child_coming_back_under_a_draining_epic_passes() {
        // The control: the same transition lands while the epic has room.
        let mut pre = full_epic_rows();
        pre.pop();
        pre.push(json!({"id": "c-16", "slug": "c-16", "title": "c-16",
                        "type": "feature", "status": "deferred", "priority": "p2",
                        "domain": "code", "parent": "e-1"}));
        let mut post = pre.clone();
        for row in post.iter_mut() {
            if crate::graph_store::entry_id(row) == Some("c-16") {
                row.as_object_mut()
                    .unwrap()
                    .insert("status".into(), json!("ready"));
            }
        }
        enforce(&pre, &post, Some(15)).unwrap();
    }

    #[test]
    fn a_non_epic_parent_is_uncapped() {
        let pre = vec![json!({"id": "f-1", "slug": "f-1", "title": "f-1",
                              "type": "feature", "status": "in_progress",
                              "priority": "p2", "domain": "code"})];
        let mut post = pre.clone();
        for i in 1..=20 {
            post.push(child(&format!("k-{i:02}"), "f-1"));
        }
        enforce(&pre, &post, Some(15)).unwrap();
    }

    #[test]
    fn a_write_is_judged_net_not_per_edge() {
        // The check reads the final state: one commit that moves a child
        // out and brings another in never exceeds the cap.
        let pre = full_epic_rows();
        let mut post = full_epic_rows();
        post.push(epic("e-2"));
        // c-01 leaves e-1 for the fresh epic e-2 (new growth of e-2: fine).
        for row in post.iter_mut() {
            if crate::graph_store::entry_id(row) == Some("c-01") {
                row.as_object_mut()
                    .unwrap()
                    .insert("parent".into(), json!("e-2"));
            }
        }
        // c-17 joins e-1, which ends at 15 open children, not 16.
        post.push(child("c-17", "e-1"));
        enforce(&pre, &post, Some(15)).unwrap();
    }

    #[test]
    fn a_blank_parent_reads_as_none() {
        let pre = full_epic_rows();
        let mut post = full_epic_rows();
        post.push(json!({"id": "c-16", "slug": "c-16", "title": "c-16",
                         "type": "feature", "status": "idea", "priority": "p2",
                         "domain": "code", "parent": ""}));
        enforce(&pre, &post, Some(15)).unwrap();
    }

    #[test]
    fn children_are_direct_not_descendants() {
        // A sub-epic counts as one child of its parent and carries its own
        // cap. Its own children never roll up into the ancestor's count:
        // e-1 sits at the cap of 15 here, and the ten grandchildren under
        // sub-1 would read 25 under a descendant count.
        let mut pre = full_epic_rows();
        pre.pop(); // 14 open children under e-1
        let mut post = pre.clone();
        post.push(json!({"id": "sub-1", "slug": "sub-1", "title": "sub-1",
                         "type": "epic", "status": "idea", "priority": "p2",
                         "domain": "code", "parent": "e-1"}));
        enforce(&pre, &post, Some(15)).unwrap();
        for i in 1..=10 {
            post.push(child(&format!("s-{i:02}"), "sub-1"));
        }
        enforce(&pre, &post, Some(15)).unwrap();
    }

    #[test]
    fn configured_cap_reads_only_positive_integers() {
        // AC3-EDGE, last line: missing file, 0, -1 and "15" all read as None.
        let dir = tempfile::tempdir().unwrap();
        let graph = dir.path().join("graph.json");
        std::fs::write(&graph, "{\n  \"entries\": []\n}\n").unwrap();
        assert_eq!(
            configured_cap(&graph),
            None,
            "no config.toml beside the graph"
        );
        for body in [
            "[backlog]\nepic_max_open_children = 0\n",
            "[backlog]\nepic_max_open_children = -1\n",
            "[backlog]\nepic_max_open_children = \"15\"\n",
            "[backlog]\nstaleness_days = 15\n",
        ] {
            std::fs::write(dir.path().join("config.toml"), body).unwrap();
            assert_eq!(configured_cap(&graph), None, "body: {body}");
        }
        std::fs::write(
            dir.path().join("config.toml"),
            "[backlog]\nepic_max_open_children = 15\n",
        )
        .unwrap();
        assert_eq!(configured_cap(&graph), Some(15));
    }
}
