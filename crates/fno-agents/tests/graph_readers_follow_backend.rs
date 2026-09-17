//! The graph.json readers follow the backend switch.
//!
//! A graph flipped to sqlite holds nodes graph.json never saw (the typed
//! api ops skip the file). Every moved caller must resolve a node the FILE
//! does not carry; the `read_defaulted` control proves the fixture really
//! split the two stores.
//!
//! Split store:
//! - x-old: written to the file pre-flip, imports into the store at flip.
//! - x-new: created through the typed api after the flip, so only graph.db
//!   carries it (the typed ops skip the file seam).

use fno_agents::backlog::api::{self, NodeCreateInput, Store};
use fno_agents::backlog::patch::{self, PatchRequest};
use fno_agents::{backlog, graph_store};

fn split_store_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let graph = root.path().join("graph.json");
    std::fs::write(
        &graph,
        serde_json::json!({"entries": [serde_json::json!({
            "id": "x-old", "title": "pre-flip", "slug": "pre-flip",
            "type": "feature", "status": "ready", "priority": "p2",
        })]})
        .to_string(),
    )
    .unwrap();
    backlog::set_backend(&graph, backlog::Backend::Sqlite).unwrap();
    let store = Store::new(&graph);
    api::node_create(
        &store,
        NodeCreateInput {
            id: "x-new".into(),
            title: "post-flip".into(),
            status: Some("ready".into()),
            priority: Some("p2".into()),
            ..Default::default()
        },
    )
    .unwrap();
    (root, graph)
}

#[test]
fn moved_callers_resolve_the_post_flip_node_the_file_lacks() {
    let (_root, graph) = split_store_fixture();

    // Control: the store carries x-new, the file does not.
    let rows = graph_store::read_rows(&graph).unwrap();
    assert!(
        rows.iter()
            .any(|e| graph_store::entry_id(e) == Some("x-new")),
        "read_rows must resolve the post-flip node"
    );
    let file_rows = graph_store::read_defaulted(&graph, false).unwrap();
    assert!(
        !file_rows
            .iter()
            .any(|e| graph_store::entry_id(e) == Some("x-new")),
        "control failed: graph.json unexpectedly holds x-new, the stores did not split"
    );
    let stored_status = rows
        .iter()
        .find(|e| graph_store::entry_id(e) == Some("x-new"))
        .and_then(|e| e.get("status"))
        .and_then(|s| s.as_str())
        .unwrap_or("MISSING")
        .to_string();
    assert_eq!(stored_status, "ready", "fixture sanity");

    // The patch door applies a --set to x-new and reads back from the store.
    let req = PatchRequest {
        node: "x-new".into(),
        status: None,
        leave: None,
        sets: vec![("domain".into(), "research".into())],
    };
    let receipt = patch::apply(&graph, &req).unwrap();
    assert_eq!(receipt.node, "x-new");
    assert!(
        receipt.changes.iter().any(|c| c.field == "domain"),
        "patch door must see x-new: {receipt:?}"
    );

    // The merge-hold door resolves x-new.
    let out = fno_agents::merge_hold::run(
        "hold-set",
        &serde_json::json!({
            "op": "hold-set", "node": "x-new", "reason": "r",
            "release_when": "w", "set_by": "s", "graph": graph.display().to_string(),
        }),
    );
    assert!(
        !out.contains("no node resolves"),
        "merge_hold must resolve x-new: {out}"
    );

    // The typed api read resolves x-new.
    let store = Store::new(&graph);
    let found = api::node(&store, "x-new").unwrap();
    assert!(found.is_some(), "api::node must resolve x-new");
}

#[test]
fn x_old_still_resolves_after_the_flip() {
    let (_root, graph) = split_store_fixture();
    let rows = graph_store::read_rows(&graph).unwrap();
    assert!(
        rows.iter()
            .any(|e| graph_store::entry_id(e) == Some("x-old")),
        "the pre-flip node must survive the flip"
    );
}
