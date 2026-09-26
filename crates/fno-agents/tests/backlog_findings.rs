//! x-26bd wave 1: the findings aggregate and its API, store-level.

use fno_agents::backlog::api::{self, FindingInput, Store};
use serde_json::json;
use std::path::PathBuf;

fn fixture(id: &str) -> serde_json::Value {
    json!({
        "id": id,
        "slug": format!("slug-{id}"),
        "title": format!("Node {id}"),
        "type": "feature",
        "status": "ready",
        "priority": "p1",
        "details": "premise",
    })
}

fn temp_graph(tag: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join(format!("{tag}-graph.json"));
    fno_agents::graph_store::seed_rows(&graph, &[fixture("x-f1"), fixture("x-f2")]).unwrap();
    (dir, graph)
}

fn input(body: &str) -> FindingInput {
    FindingInput {
        body: body.to_string(),
        ..Default::default()
    }
}

#[test]
fn finding_create_readback_and_list_roundtrip() {
    let (_dir, graph) = temp_graph("create");
    let store = Store::new(&graph);
    let receipt = api::finding_create(&store, "x-f1", input("gate holds")).unwrap();
    assert_eq!(receipt.finding_id.len(), 8);
    assert_eq!(receipt.node_id, "x-f1");

    // A second store instance reads the same id as open (AC1 readback).
    let later = api::findings(&Store::new(&graph), Some("x-f1"), true).unwrap();
    assert_eq!(later.len(), 1);
    assert_eq!(later[0].finding_id, receipt.finding_id);
    assert_eq!(later[0].body, "gate holds");
    assert!(later[0].resolved_at.is_none());

    // Resolve clears it from the open list, keeps it in the full list.
    let resolve = api::finding_resolve(&store, &receipt.finding_id, Some("sess-1")).unwrap();
    assert_eq!(resolve.status, "resolved");
    let open = api::findings(&store, Some("x-f1"), true).unwrap();
    assert!(open.is_empty());
    let all = api::findings(&store, Some("x-f1"), false).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].resolved_by_session_id.as_deref(), Some("sess-1"));
}

#[test]
fn finding_create_refuses_empty_over_limit_and_unknown_node() {
    let (_dir, graph) = temp_graph("refuse");
    let store = Store::new(&graph);

    let empty = api::finding_create(&store, "x-f1", input("   \n  "));
    assert!(empty.is_err());

    let long = "x".repeat(5001);
    let over = api::finding_create(&store, "x-f1", input(&long));
    assert!(over.is_err(), "5001 chars must refuse");
    let at_limit = api::finding_create(&store, "x-f1", input(&"x".repeat(5000)));
    assert!(at_limit.is_ok(), "5000 chars must pass");

    let unknown = api::finding_create(&store, "x-none", input("ghost"));
    assert!(unknown.is_err());
}

#[test]
fn finding_resolve_unknown_id_is_an_error() {
    let (_dir, graph) = temp_graph("resolve-unknown");
    let store = Store::new(&graph);
    let error = api::finding_resolve(&store, "nope1234", Some("sess")).unwrap_err();
    assert!(error.0.contains("nope1234"), "{:?}", error.0);
}

#[test]
fn second_resolve_reports_already_resolved() {
    let (_dir, graph) = temp_graph("already");
    let store = Store::new(&graph);
    let receipt = api::finding_create(&store, "x-f1", input("first")).unwrap();
    let first = api::finding_resolve(&store, &receipt.finding_id, Some("s1")).unwrap();
    let second = api::finding_resolve(&store, &receipt.finding_id, Some("s2")).unwrap();
    assert_eq!(second.status, "already_resolved");
    assert_eq!(second.resolved_at, first.resolved_at);
}

#[test]
fn findings_survive_store_reopen() {
    let (_dir, graph) = temp_graph("flip");
    let store = Store::new(&graph);
    let receipt = api::finding_create(&store, "x-f1", input("survives")).unwrap();
    api::finding_resolve(&store, &receipt.finding_id, Some("s1")).unwrap();

    // A second store instance reads the same id, body and resolution.
    let reread = api::findings(&Store::new(&graph), Some("x-f1"), false).unwrap();
    assert_eq!(reread.len(), 1);
    assert_eq!(reread[0].finding_id, receipt.finding_id);
    assert_eq!(reread[0].body, "survives");
    assert!(reread[0].resolved_at.is_some());
}

#[test]
fn findings_table_rows_carry_the_typed_columns() {
    let (_dir, graph) = temp_graph("table");
    let store = Store::new(&graph);
    let _receipt = api::finding_create(
        &store,
        "x-f1",
        FindingInput {
            body: "holds the gate".into(),
            block_cmd: Some("fno do pr merge 1".into()),
            block_excerpt: Some("excerpt".into()),
            source_session_id: Some("s-9".into()),
            source_harness: Some("claude".into()),
        },
    )
    .unwrap();
    let connection_rows = fno_agents::graph_store::read_rows(&graph).unwrap();
    let row = connection_rows
        .iter()
        .find(|r| r.get("id").and_then(serde_json::Value::as_str) == Some("x-f1"))
        .unwrap();
    let stored = row
        .get("findings")
        .and_then(serde_json::Value::as_array)
        .unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(
        stored[0]
            .get("block_cmd")
            .and_then(serde_json::Value::as_str),
        Some("fno do pr merge 1")
    );
    assert_eq!(
        stored[0]
            .get("source_harness")
            .and_then(serde_json::Value::as_str),
        Some("claude")
    );
    assert!(
        stored[0].get("resolved_at").is_none(),
        "an open finding omits resolved_at"
    );
}

#[test]
fn over_limit_excerpt_is_capped_not_refused() {
    let (_dir, graph) = temp_graph("excerpt");
    let store = Store::new(&graph);
    let receipt = api::finding_create(
        &store,
        "x-f1",
        FindingInput {
            body: "capped excerpt".into(),
            block_excerpt: Some("e".repeat(3000)),
            ..Default::default()
        },
    )
    .unwrap();
    let list = api::findings(&store, Some("x-f1"), false).unwrap();
    assert_eq!(
        list[0]
            .block_excerpt
            .as_deref()
            .map(|s| s.chars().count())
            .unwrap_or(0),
        2048
    );
    let _ = receipt;
}
