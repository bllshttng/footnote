//! What one `fno agents list` row shows beyond registry storage: the node's PR
//! and the basis-qualified MODEL and PR cells. A field with a basis shows the
//! basis beside it; no cell is blanked to avoid misleading.

use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// The primary PR of each named node, from ONE graph read. `None` when the
/// graph could not be read, so every row can word that instead of failing.
pub fn node_primary_prs<'a>(
    graph: &Path,
    nodes: impl IntoIterator<Item = &'a str>,
) -> Option<HashMap<String, Option<i64>>> {
    let wanted: HashSet<&str> = nodes.into_iter().collect();
    if wanted.is_empty() {
        return Some(HashMap::new());
    }
    let rows = crate::graph_store::read_rows(graph).ok()?;
    Some(
        rows.iter()
            .filter_map(|row| {
                let id = row.get("id").and_then(Value::as_str)?;
                wanted.contains(id).then(|| {
                    let pr = crate::king_board::prs::node_pr_refs(row)
                        .first()
                        .map(|(n, _)| *n);
                    (id.to_string(), pr)
                })
            })
            .collect(),
    )
}

/// The row's `pr` and `pr_basis`: `node`, `no-node`, `no-pr` or
/// `graph-unreadable`. A node the graph does not hold reads `no-pr`.
pub fn pr_pair(
    node: Option<&str>,
    prs: Option<&HashMap<String, Option<i64>>>,
) -> (Option<i64>, &'static str) {
    let Some(node) = node.filter(|n| !n.is_empty()) else {
        return (None, "no-node");
    };
    match prs {
        None => (None, "graph-unreadable"),
        Some(map) => match map.get(node).copied().flatten() {
            Some(n) => (Some(n), "node"),
            None => (None, "no-pr"),
        },
    }
}

fn nonempty<'a>(row: &'a Value, key: &str) -> Option<&'a str> {
    row.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// The MODEL cell. The observation wins over the stored request, and every
/// value names the basis it was read from.
pub fn model_cell(row: &Value) -> String {
    let observed = row
        .get("observed_model")
        .filter(|m| m.get("kind").and_then(Value::as_str) == Some("observed"))
        .and_then(|m| nonempty(m, "model"));
    if let Some(observed) = observed {
        let substituted = row.get("model_substituted").is_some_and(|m| !m.is_null());
        return match nonempty(row, "requested_model") {
            Some(requested) if substituted => {
                format!("{observed} (observed; requested {requested})")
            }
            _ => format!("{observed} (observed)"),
        };
    }
    if let Some(model) = nonempty(row, "model") {
        let basis = nonempty(row, "model_basis").unwrap_or("requested");
        return format!("{model} ({basis})");
    }
    match nonempty(row, "requested_model") {
        Some(requested) => format!("{requested} (requested)"),
        None => "- (unrequested)".to_string(),
    }
}

/// The PR cell, worded from `pr_basis`.
pub fn pr_cell(row: &Value) -> String {
    match (
        row.get("pr").and_then(Value::as_i64),
        nonempty(row, "pr_basis"),
    ) {
        (Some(n), _) => format!("#{n}"),
        (None, Some("no-pr")) => "none".to_string(),
        (None, Some("no-node")) => "- (no-node)".to_string(),
        (None, Some("graph-unreadable")) => "? (graph-unreadable)".to_string(),
        (None, Some(other)) => format!("? ({other})"),
        (None, None) => "? (unmeasured)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn model_cell_names_its_basis() {
        let requested = json!({"model": "glm-5.3-flash[1m]", "model_basis": "requested",
            "observed_model": {"kind": "no-transcript"}});
        assert_eq!(model_cell(&requested), "glm-5.3-flash[1m] (requested)");
        let observed = json!({"model": "opus", "model_basis": "requested",
            "observed_model": {"kind": "observed", "model": "glm-5.3-flash"}});
        assert_eq!(model_cell(&observed), "glm-5.3-flash (observed)");
        let substituted = json!({"requested_model": "glm-5.3-flash[1m]",
            "model_substituted": {"requested": "glm", "observed": "claude"},
            "observed_model": {"kind": "observed", "model": "claude-opus-5"}});
        assert_eq!(
            model_cell(&substituted),
            "claude-opus-5 (observed; requested glm-5.3-flash[1m])"
        );
        let request_only = json!({"requested_model": "gpt-5.6-luna", "observed_model": null});
        assert_eq!(model_cell(&request_only), "gpt-5.6-luna (requested)");
        let nothing = json!({"model": null, "requested_model": null,
            "observed_model": {"kind": "no-model-yet"}});
        assert_eq!(model_cell(&nothing), "- (unrequested)");
    }

    #[test]
    fn pr_pair_words_every_absence() {
        let map: HashMap<String, Option<i64>> =
            [("x-1".to_string(), Some(2136)), ("x-2".to_string(), None)].into();
        assert_eq!(pr_pair(Some("x-1"), Some(&map)), (Some(2136), "node"));
        assert_eq!(pr_pair(Some("x-2"), Some(&map)), (None, "no-pr"));
        assert_eq!(pr_pair(Some("x-9"), Some(&map)), (None, "no-pr"));
        assert_eq!(pr_pair(None, Some(&map)), (None, "no-node"));
        assert_eq!(pr_pair(Some("x-1"), None), (None, "graph-unreadable"));
        assert_eq!(pr_cell(&json!({"pr": 2136, "pr_basis": "node"})), "#2136");
        assert_eq!(
            pr_cell(&json!({"pr": null, "pr_basis": "no-node"})),
            "- (no-node)"
        );
        assert_eq!(pr_cell(&json!({"pr": null, "pr_basis": "no-pr"})), "none");
        assert_eq!(
            pr_cell(&json!({"pr": null, "pr_basis": "graph-unreadable"})),
            "? (graph-unreadable)"
        );
    }

    #[test]
    fn node_primary_prs_reads_the_graph_once() {
        let dir = std::env::temp_dir().join(format!("fno-list-row-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let graph = dir.join("graph.json");
        std::fs::write(
            &graph,
            json!({"entries": [
                {"id": "x-1", "title": "a", "status": "ready", "pr_number": 2136},
                {"id": "x-2", "title": "b", "status": "ready"}
            ]})
            .to_string(),
        )
        .unwrap();
        let map = node_primary_prs(&graph, ["x-1", "x-2"]).expect("graph reads");
        assert_eq!(map.get("x-1"), Some(&Some(2136)));
        assert_eq!(map.get("x-2"), Some(&None));
        std::fs::remove_dir_all(&dir).ok();
    }
}
