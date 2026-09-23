//! The unplanned idea cap. The default bounds machine-filed idea growth in
//! each nearest-epic or project scope, while operator asks remain uncapped.

use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

pub const DEFAULT_MAX_OPEN_IDEAS: usize = 25;

pub fn configured_cap(graph: &Path) -> (Option<usize>, &'static str) {
    let dir = graph.parent().unwrap_or_else(|| Path::new(""));
    match crate::backlog_ready::backlog_config_int(dir, "max_open_ideas") {
        Some(0) => (None, "off"),
        Some(value) if value > 0 => (Some(value as usize), "config"),
        _ => (Some(DEFAULT_MAX_OPEN_IDEAS), "default"),
    }
}

fn capped_idea(row: &Value) -> bool {
    row.get("status").and_then(Value::as_str) == Some("idea")
        && row
            .get("plan_path")
            .map(|value| value.is_null() || value.as_str() == Some(""))
            .unwrap_or(true)
        && row.get("type").and_then(Value::as_str) != Some("epic")
        && row
            .get("group_slug")
            .map(|value| value.is_null() || value.as_str() == Some(""))
            .unwrap_or(true)
        && row.get("source_kind").and_then(Value::as_str) != Some("operator_request")
}

fn scope_for(row: &Value, rows_by_id: &std::collections::HashMap<&str, &Value>) -> String {
    let project = row.get("project").and_then(Value::as_str).unwrap_or("");
    let mut current = row;
    for _ in 0..64 {
        let Some(parent) = current
            .get("parent")
            .and_then(Value::as_str)
            .filter(|parent| !parent.is_empty())
        else {
            break;
        };
        let Some(parent_row) = rows_by_id.get(parent).copied() else {
            break;
        };
        if parent_row.get("type").and_then(Value::as_str) == Some("epic") {
            return format!("epic:{parent}");
        }
        current = parent_row;
    }
    format!("project:{project}")
}

pub fn enforce(pre: &[Value], post: &[Value], cap: Option<usize>) -> Result<(), String> {
    let Some(cap) = cap else {
        return Ok(());
    };
    let pre_by_id = crate::graph_store::index_by_id(pre);
    let post_by_id = crate::graph_store::index_by_id(post);
    let mut births: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for row in post {
        if !capped_idea(row) {
            continue;
        }
        let scope = scope_for(row, &post_by_id);
        *counts.entry(scope.clone()).or_default() += 1;
        if let Some(id) = crate::graph_store::entry_id(row) {
            if !pre_by_id.contains_key(id) {
                births.entry(scope).or_default().push(row);
            }
        }
    }
    for (scope, mut new_rows) in births {
        let Some(&open_ideas) = counts.get(&scope) else {
            continue;
        };
        if open_ideas <= cap {
            continue;
        }
        new_rows.sort_by_key(|row| crate::graph_store::entry_id(row).unwrap_or(""));
        let row = new_rows[0];
        let id = crate::graph_store::entry_id(row).unwrap_or("<missing-id>");
        let title: String = row
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(80)
            .collect();
        let mut oldest: Vec<&Value> = post
            .iter()
            .filter(|candidate| {
                capped_idea(candidate) && scope_for(candidate, &post_by_id) == scope
            })
            .collect();
        oldest.sort_by(|left, right| {
            left.get("created_at")
                .and_then(Value::as_str)
                .unwrap_or("\u{10ffff}")
                .cmp(
                    right
                        .get("created_at")
                        .and_then(Value::as_str)
                        .unwrap_or("\u{10ffff}"),
                )
                .then_with(|| {
                    crate::graph_store::entry_id(left)
                        .unwrap_or("")
                        .cmp(crate::graph_store::entry_id(right).unwrap_or(""))
                })
        });
        let oldest_ids = oldest
            .iter()
            .take(3)
            .filter_map(|candidate| crate::graph_store::entry_id(candidate))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "idea cap: refusing to file '{id}' ('{title}') in {scope}: it would hold {open_ideas} unplanned ideas and the cap is {cap} (backlog.max_open_ideas). No node was minted. Fold the finding into the open node it belongs to: fno backlog idea \"{title}\" --wave-of <node-id> --difficulty <low|medium|high>. Or put it under an epic with room: --parent <epic-id>. Or free room by closing the oldest unplanned ideas in {scope}: {oldest_ids}. An operator ask files with --source-kind operator_request and is not capped."
        ));
    }
    Ok(())
}

pub fn idea_load(rows: &[Value], cap: Option<usize>) -> Vec<Value> {
    let by_id = crate::graph_store::index_by_id(rows);
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for row in rows {
        if capped_idea(row) {
            *counts.entry(scope_for(row, &by_id)).or_default() += 1;
        }
    }
    let mut load: Vec<(String, usize)> = counts.into_iter().collect();
    load.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    load.into_iter()
        .map(|(scope, open_ideas)| {
            serde_json::json!({
                "scope": scope,
                "open_ideas": open_ideas,
                "full": cap.is_some_and(|limit| open_ideas >= limit)
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn idea(id: &str, project: &str, created_at: &str) -> Value {
        json!({
            "id": id, "slug": id, "title": format!("title {id}"),
            "type": "feature", "status": "idea", "project": project,
            "created_at": created_at
        })
    }

    fn full_rows() -> Vec<Value> {
        (1..=25)
            .map(|i| {
                idea(
                    &format!("x-{i:02}"),
                    "p",
                    &format!("2026-01-{i:02}T00:00:00Z"),
                )
            })
            .collect()
    }

    #[test]
    fn idea_cap_refuses_the_26th_birth_and_names_oldest_ids() {
        let pre = full_rows();
        let mut post = pre.clone();
        post.push(idea("x-26", "p", "2026-02-01T00:00:00Z"));
        let error = enforce(&pre, &post, Some(25)).unwrap_err();
        assert!(error.contains("idea cap:"), "{error}");
        assert!(error.contains("26 unplanned ideas"), "{error}");
        assert!(error.contains("backlog.max_open_ideas"), "{error}");
        assert!(error.contains("--wave-of"), "{error}");
        assert!(error.contains("x-01, x-02, x-03"), "{error}");
    }

    #[test]
    fn idea_cap_exempts_operator_planned_epic_and_grouped_rows() {
        let pre = full_rows();
        for extra in [
            json!({"id": "operator", "status": "idea", "project": "p", "source_kind": "operator_request"}),
            json!({"id": "planned", "status": "idea", "project": "p", "plan_path": "plans/x.md"}),
            json!({"id": "epic", "status": "idea", "project": "p", "type": "epic"}),
            json!({"id": "grouped", "status": "idea", "project": "p", "group_slug": "split"}),
        ] {
            let mut post = pre.clone();
            post.push(extra);
            enforce(&pre, &post, Some(25)).unwrap();
        }
    }

    #[test]
    fn idea_cap_judges_births_only() {
        let mut pre = full_rows();
        pre.push(idea("deferred", "p", "2026-02-01T00:00:00Z"));
        pre.last_mut().unwrap()["status"] = json!("deferred");
        let mut post = pre.clone();
        post.last_mut().unwrap()["status"] = json!("idea");
        enforce(&pre, &post, Some(25)).unwrap();
    }

    #[test]
    fn configured_cap_defaults_turns_off_and_rejects_bad_values() {
        let missing = tempfile::tempdir().unwrap();
        assert_eq!(
            configured_cap(&missing.path().join("graph.json")),
            (Some(25), "default")
        );

        for contents in [
            "[backlog]\nmax_open_ideas = \"25\"\n",
            "[backlog]\nmax_open_ideas = -3\n",
            "not valid toml",
        ] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("config.toml"), contents).unwrap();
            assert_eq!(
                configured_cap(&dir.path().join("graph.json")),
                (Some(25), "default")
            );
        }

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "[backlog]\nmax_open_ideas = 0\n",
        )
        .unwrap();
        assert_eq!(
            configured_cap(&dir.path().join("graph.json")),
            (None, "off")
        );
    }

    #[test]
    fn nested_ideas_use_the_nearest_epic_and_cycles_are_bounded() {
        let mut rows = vec![
            json!({"id": "e-1", "type": "epic"}),
            json!({"id": "f-1", "type": "feature", "parent": "e-1"}),
        ];
        rows.push(json!({"id": "x-1", "status": "idea", "project": "p", "parent": "f-1"}));
        let by_id = crate::graph_store::index_by_id(&rows);
        assert_eq!(scope_for(&rows[2], &by_id), "epic:e-1");

        for i in 0..64 {
            let parent = if i == 63 {
                "cycle-0".to_string()
            } else {
                format!("cycle-{}", i + 1)
            };
            rows.push(json!({"id": format!("cycle-{i}"), "status": "idea", "project": "p", "parent": parent}));
        }
        let by_id = crate::graph_store::index_by_id(&rows);
        assert_eq!(scope_for(rows.last().unwrap(), &by_id), "project:p");
    }

    #[test]
    fn idea_load_orders_fullest_scopes_first() {
        let mut rows = full_rows();
        rows.extend((1..=2).map(|i| idea(&format!("q-{i}"), "q", "2026-01-01T00:00:00Z")));
        let load = idea_load(&rows, Some(25));
        assert_eq!(
            load[0],
            json!({"scope": "project:p", "open_ideas": 25, "full": true})
        );
        assert_eq!(
            load[1],
            json!({"scope": "project:q", "open_ideas": 2, "full": false})
        );
    }
}
