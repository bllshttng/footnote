//! The session-to-node join the top view reads: one client-side read over
//! the graph's `sessions[]` rows and the node claims, keyed by harness
//! session id, a live claim outranking the graph record. Split from the
//! Python top view under the file budget; the answer is one JSON object.

use crate::paths::AgentsHome;
use serde_json::{json, Map, Value};
use std::collections::HashMap;

pub fn run_sessions_map(_home: &AgentsHome) -> i32 {
    let mut out: Map<String, Value> = Map::new();
    let mut node_pr: HashMap<String, Option<i64>> = HashMap::new();

    // Graph rows first: the lower-precedence answer, and the pr_number
    // source a claim's node reuses.
    if let Ok(rows) = crate::graph_store::read_rows_where(
        &crate::graph_get::default_graph_path(),
        &crate::backlog::RowQuery {
            fields: Some(
                ["id", "sessions", "pr_number"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            ),
            with_blockers: true,
            ..Default::default()
        },
    ) {
        for row in &rows {
            let Some(node) = row.get("id").and_then(Value::as_str) else {
                continue;
            };
            node_pr.insert(
                node.to_ascii_lowercase(),
                row.get("pr_number").and_then(Value::as_i64),
            );
            let Some(sessions) = row.get("sessions").and_then(Value::as_array) else {
                continue;
            };
            for s in sessions {
                let Some(sid) = s
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                else {
                    continue;
                };
                out.entry(sid.to_ascii_lowercase()).or_insert_with(|| {
                    let pr = row.get("pr_number").and_then(Value::as_i64);
                    json!({
                        "node": node,
                        "pr": pr,
                        "pr_basis": if pr.is_some() { "node" } else { "no-pr" },
                        "basis": "graph",
                    })
                });
            }
        }
    }

    // A live node claim is the session's own work order: it overwrites.
    if let Ok(answer) = crate::claim_store::list_db(Some("node:"), true, None) {
        for record in answer
            .get("rows")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(node) = record
                .get("key")
                .and_then(Value::as_str)
                .and_then(|k| k.strip_prefix("node:"))
                .filter(|n| !n.is_empty())
            else {
                continue;
            };
            let Some(sid) = record
                .get("holder")
                .and_then(Value::as_str)
                .and_then(|h| h.strip_prefix("target-session:"))
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let pr = node_pr.get(&node.to_ascii_lowercase()).copied().flatten();
            out.insert(
                sid.to_ascii_lowercase(),
                json!({
                    "node": node,
                    "pr": pr,
                    "pr_basis": if pr.is_some() { "node" } else { "no-pr" },
                    "basis": "claim",
                }),
            );
        }
    }

    println!("{}", Value::Object(out));
    0
}
