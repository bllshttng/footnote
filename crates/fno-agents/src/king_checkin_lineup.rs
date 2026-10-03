//! The `lineup:` table: seated court rows then the on-deck queue, one row
//! each with title, difficulty, PR, harness/model and status.
//!
//! Split out of `king_checkin` so the row builder stays testable without a
//! beat: `lineup_rows` folds the inputs, `render_lineup` prints them. The
//! planned model for a queued row comes in as a closure so this module never
//! reads config itself.

use crate::king_checkin::{dash, MAX_COURT_ROWS};
use crate::state::Registry;
use serde_json::{json, Value};

/// The reads `render_lines` needs to build the lineup. A failed graph read
/// replaces the table with `READER FAILED lineup:`; a failed registry read
/// prints the line and keeps the table with `-` model cells.
pub(crate) struct LineupSources<'a> {
    pub(crate) queue: &'a [String],
    pub(crate) graph: Result<&'a [Value], &'a str>,
    pub(crate) registry: Option<&'a Registry>,
    pub(crate) registry_error: Option<&'a str>,
    pub(crate) planned: &'a dyn Fn(&str) -> Option<String>,
}

static NO_PLANNED: fn(&str) -> Option<String> = |_| None;

impl<'a> LineupSources<'a> {
    pub(crate) fn empty() -> Self {
        LineupSources {
            queue: &[],
            graph: Ok(&[]),
            registry: None,
            registry_error: None,
            planned: &NO_PLANNED,
        }
    }
}

fn graph_row<'a>(graph_rows: &'a [Value], id: &str) -> Option<&'a Value> {
    graph_rows
        .iter()
        .find(|g| g.get("id").and_then(Value::as_str) == Some(id))
}

/// The seated model: the registry entry whose `node` equals the id, as
/// `harness/model`; a seat the registry never recorded reads `-`.
fn seated_model(registry: Option<&Registry>, id: &str) -> Option<String> {
    let entry = registry?
        .entries
        .iter()
        .find(|e| e.node.as_deref() == Some(id))?;
    match (entry.harness.as_deref(), entry.model.as_deref()) {
        (Some(h), Some(m)) if !h.is_empty() && !m.is_empty() => Some(format!("{h}/{m}")),
        _ => None,
    }
}

/// Seated rows first in court order, then the queue in queue order, skipping
/// a queued id that is already seated. Title, difficulty and PR come from the
/// graph row; a queued row's model is the planned seat for its difficulty.
pub(crate) fn lineup_rows(
    court_rows: &[Value],
    queue: &[String],
    graph_rows: &[Value],
    registry: Option<&Registry>,
    planned: impl Fn(&str) -> Option<String>,
) -> Vec<Value> {
    let mut rows: Vec<Value> = Vec::new();
    let mut seated: Vec<String> = Vec::new();
    for row in court_rows {
        let id = row.get("id").and_then(Value::as_str).unwrap_or("");
        seated.push(id.to_string());
        let graph = graph_row(graph_rows, id);
        rows.push(json!({
            "node": row.get("id"),
            "title": graph.and_then(|g| g.get("title")),
            "difficulty": graph.and_then(|g| g.get("difficulty")),
            "pr": graph
                .and_then(|g| g.get("pr_number"))
                .or_else(|| row.get("pr_number")),
            "model": seated_model(registry, id),
            "status": row.get("status"),
        }));
    }
    for id in queue {
        if seated.iter().any(|s| s == id) {
            continue;
        }
        let graph = graph_row(graph_rows, id);
        let difficulty = graph
            .and_then(|g| g.get("difficulty"))
            .and_then(Value::as_str);
        rows.push(json!({
            "node": id,
            "title": graph.and_then(|g| g.get("title")),
            "difficulty": graph.and_then(|g| g.get("difficulty")),
            "pr": graph.and_then(|g| g.get("pr_number")),
            "model": difficulty.and_then(planned),
            "status": "on deck",
        }));
    }
    rows
}

fn cell(v: Option<&Value>) -> String {
    dash(v).replace('|', "\\|")
}

/// `lineup:` then one markdown row per entry, capped like the court rows.
pub(crate) fn render_lineup(rows: &[Value]) -> Vec<String> {
    let mut out = vec![
        "lineup:".to_string(),
        "| node | title | difficulty | PR | harness/model | status |".to_string(),
        "|---|---|---|---|---|---|".to_string(),
    ];
    for row in rows.iter().take(MAX_COURT_ROWS) {
        out.push(format!(
            "| {} | {} | {} | {} | {} | {} |",
            cell(row.get("node")),
            cell(row.get("title")),
            cell(row.get("difficulty")),
            cell(row.get("pr")),
            cell(row.get("model")),
            cell(row.get("status")),
        ));
    }
    let hidden = rows.len().saturating_sub(MAX_COURT_ROWS);
    if hidden > 0 {
        out.push(format!("  ... {hidden} more rows cut"));
    }
    out
}
