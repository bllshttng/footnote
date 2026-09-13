//! x-920a wave 1: bounded state + durable history, real-store tests (AC1-AC4).

use fno_agents::backlog::node_state::{self, StateWriteInput};
use serde_json::json;
use std::path::PathBuf;

fn fixture_node(id: &str, details: &str) -> serde_json::Value {
    json!({
        "id": id,
        "slug": format!("slug-{id}"),
        "title": format!("Node {id}"),
        "type": "feature",
        "status": "ready",
        "priority": "p1",
        "details": details,
    })
}

fn write_graph(path: &PathBuf, entries: &[serde_json::Value]) {
    std::fs::write(
        path,
        serde_json::to_string(&json!({ "entries": entries })).unwrap(),
    )
    .unwrap();
}

fn ws(_graph: &std::path::Path, id: &str, body: &str) -> StateWriteInput {
    StateWriteInput {
        node_id: id.to_string(),
        body: body.to_string(),
        if_revision: None,
        source_session_id: None,
        source_harness: None,
    }
}

fn read_graph(path: &PathBuf) -> Vec<serde_json::Value> {
    let raw = std::fs::read_to_string(path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    v["entries"].as_array().unwrap().clone()
}

#[test]
fn ac1_one_current_state_priors_in_history() {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    write_graph(&graph, &[fixture_node("t-1", "premise")]);
    for i in 0..100 {
        node_state::replace_state(&graph, &ws(&graph, "t-1", &format!("state body {i}"))).unwrap();
    }
    let row = read_graph(&graph)
        .into_iter()
        .find(|r| r["id"] == json!("t-1"))
        .unwrap();
    let st = row.get(node_state::STATE_KEY).unwrap();
    assert_eq!(st["revision"], json!(100));
    assert_eq!(st["body"], json!("state body 99"));
    let (records, total) = node_state::history_page(&graph, "t-1", 0, 500).unwrap();
    assert_eq!(total, 99);
    assert_eq!(records.len(), 99);
    for (i, rec) in records.iter().enumerate() {
        assert_eq!(rec["prior_revision"], json!(i as u64 + 1));
        assert_eq!(rec["original"]["body"], json!(format!("state body {i}")));
    }
}

/// A raw mutation through the shared seam (what any unrelated writer does).
fn raw_mutate(graph: &PathBuf, rows: Vec<serde_json::Value>) {
    fno_agents::graph_store::locked_mutate(
        graph,
        fno_agents::graph_store::MutateInput {
            entries: rows,
            canonical_path: None,
            base_version: None,
            plan_rungs: None,
        },
        std::time::Duration::from_secs(5),
    )
    .unwrap();
}

#[test]
fn ac2_budget_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    write_graph(
        &graph,
        &[json!({
            "id": "b-1", "slug": "slug-b-1", "title": "n", "type": "feature",
            "status": "ready", "priority": "p1", "details": "x".repeat(4999),
        })],
    );
    // 5000 total: first write succeeds.
    node_state::replace_state(&graph, &ws(&graph, "b-1", "y")).unwrap();
    assert_eq!(node_state::current_revision(&graph, "b-1").unwrap(), 1);
    // 5001 total refuses with the accounting line.
    let err = node_state::replace_state(&graph, &ws(&graph, "b-1", &"y".repeat(2)))
        .unwrap_err()
        .to_string();
    assert!(err.contains("total=5001"), "message: {err}");
    assert!(err.contains("limit=5000"), "message: {err}");
    // Multibyte emoji count per scalar (not bytes): details 3000 + 2001
    // emoji body = 5001 scalars, refused.
    write_graph(
        &graph,
        &[json!({
            "id": "b-1", "slug": "slug-b-1", "title": "n", "type": "feature",
            "status": "ready", "priority": "p1", "details": "x".repeat(3000),
        })],
    );
    let err = node_state::replace_state(&graph, &ws(&graph, "b-1", &"🌊".repeat(2001)))
        .unwrap_err()
        .to_string();
    assert!(err.contains("total=5001"), "message: {err}");
}

#[test]
fn ac3_oversized_legacy_row_rules() {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    // Legacy oversized row (details 6000 chars, no state).
    write_graph(
        &graph,
        &[json!({
            "id": "o-1", "slug": "slug-o-1", "title": "n", "type": "feature",
            "status": "ready", "priority": "p1", "details": "x".repeat(6000),
        })],
    );
    // Unrelated status change on the oversized row still succeeds.
    let mut rows = read_graph(&graph);
    for r in rows.iter_mut() {
        if r["id"] == json!("o-1") {
            r["status"] = json!("in_progress");
        }
    }
    raw_mutate(&graph, rows);
    let row = read_graph(&graph)
        .into_iter()
        .find(|r| r["id"] == json!("o-1"))
        .unwrap();
    assert_eq!(row["status"], json!("in_progress"));
    // Any state write on the oversized row now grows the total (6000 -> more)
    // and refuses, even for a one-character body.
    let err = node_state::replace_state(&graph, &ws(&graph, "o-1", "y"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("total=6001"), "message: {err}");
    // A details-only edit that REDUCES the oversized row is legal through the
    // seam (unrelated to state), and leaves the row smaller.
    let mut rows = read_graph(&graph);
    for r in rows.iter_mut() {
        if r["id"] == json!("o-1") {
            r["details"] = json!("x".repeat(3000));
        }
    }
    raw_mutate(&graph, rows);
    let row = read_graph(&graph)
        .into_iter()
        .find(|r| r["id"] == json!("o-1"))
        .unwrap();
    assert_eq!(node_state::prose_total(&row), 3000);
}

fn ws_rev(_graph: &std::path::Path, id: &str, body: &str, if_revision: u64) -> StateWriteInput {
    StateWriteInput {
        node_id: id.to_string(),
        body: body.to_string(),
        if_revision: Some(if_revision),
        source_session_id: None,
        source_harness: None,
    }
}

#[test]
fn ac4_conflict_refuses_stale_writer() {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    write_graph(&graph, &[fixture_node("c-1", "d")]);
    node_state::replace_state(&graph, &ws(&graph, "c-1", "v1")).unwrap();
    let rev1 = node_state::current_revision(&graph, "c-1").unwrap();
    node_state::replace_state(&graph, &ws(&graph, "c-1", "v2")).unwrap();
    let err = node_state::replace_state(&graph, &ws_rev(&graph, "c-1", "stale", rev1))
        .unwrap_err()
        .to_string();
    assert!(err.contains("state conflict"), "message: {err}");
    assert!(err.contains("re-read"), "message: {err}");
}

#[test]
fn ac4_history_failure_preserves_state() {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    write_graph(&graph, &[fixture_node("c-1", "d")]);
    node_state::replace_state(&graph, &ws(&graph, "c-1", "v1")).unwrap();
    node_state::replace_state(&graph, &ws(&graph, "c-1", "v2")).unwrap();
    let hist_dir = dir.path().join("graph.json.history");
    std::fs::remove_file(hist_dir.join("notes.jsonl")).unwrap();
    std::fs::create_dir_all(hist_dir.join("notes.jsonl")).unwrap();
    let err = node_state::replace_state(&graph, &ws(&graph, "c-1", "v3"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("history write failed"), "message: {err}");
    let row = read_graph(&graph)
        .into_iter()
        .find(|r| r["id"] == json!("c-1"))
        .unwrap();
    assert_eq!(row[node_state::STATE_KEY]["body"], json!("v2"));
    std::fs::remove_dir_all(hist_dir).unwrap();
    node_state::replace_state(&graph, &ws(&graph, "c-1", "v3")).unwrap();
    let (_, h) = node_state::history_page(&graph, "c-1", 0, 100).unwrap();
    assert_eq!(h, 1, "only v2@1 remains journaled; failed write added none");
}

#[test]
fn history_dedupe_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    write_graph(&graph, &[fixture_node("h-1", "d")]);
    let original = json!({"body": "prior", "revision": 7});
    for _ in 0..3 {
        fno_agents::backlog::note_history::append(
            &graph,
            "h-1",
            fno_agents::backlog::note_history::REASON_STATE_REPLACED,
            Some(7),
            &original,
            None,
            None,
        )
        .unwrap();
    }
    let (_, h) = fno_agents::backlog::note_history::read(&graph, Some("h-1"), 0, 100).unwrap();
    assert_eq!(h, 1, "identical records dedupe to one");
}
