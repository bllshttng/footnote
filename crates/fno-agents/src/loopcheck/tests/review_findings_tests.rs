use super::*;

#[test]
fn store_findings_open_blocks_and_resolved_does_not() {
    // AC4: an open finding reaches the gate's view; a resolve clears it.
    let tmp = tempfile::tempdir().unwrap();
    let graph = tmp.path().join("graph.json");
    crate::graph_store::seed_rows(
        &graph,
        &[serde_json::json!({
            "id": "x-1", "slug": "x-1", "title": "n", "type": "feature",
            "status": "ready", "priority": "p1"
        })],
    )
    .unwrap();
    let store = crate::backlog::api::Store::new(&graph);
    let receipt = crate::backlog::api::finding_create(
        &store,
        "x-1",
        crate::backlog::api::FindingInput {
            body: "off-by-one in the loop\nsecond line".into(),
            ..Default::default()
        },
    )
    .unwrap();
    let (open, error) = open_findings_from_store(&graph, "x-1");
    assert!(error.is_none());
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].id, receipt.finding_id);
    assert_eq!(open[0].first_line, "off-by-one in the loop"); // first line only

    crate::backlog::api::finding_resolve(&store, &receipt.finding_id, Some("s1")).unwrap();
    let (open, error) = open_findings_from_store(&graph, "x-1");
    assert!(error.is_none());
    assert!(open.is_empty(), "a resolved finding never gates");
}

#[test]
fn store_findings_read_error_is_named_not_zero() {
    // AC5: a store whose JSON leg is unparseable (and no db to answer
    // instead) reads as an ERROR, never as a clean node.
    let tmp = tempfile::tempdir().unwrap();
    let graph = tmp.path().join("graph.json");
    std::fs::write(&graph, "{ not json at all").unwrap();
    let (open, error) = open_findings_from_store(&graph, "x-1");
    assert!(open.is_empty());
    assert!(error.is_some(), "could-not-read must not read as zero");
}

#[test]
fn review_finding_block_reason_quotes_first_plus_count() {
    let open = vec![
        OpenFinding {
            id: "aaa".into(),
            first_line: "the bug".into(),
        },
        OpenFinding {
            id: "bbb".into(),
            first_line: "another".into(),
        },
    ];
    let r = build_findings_block_reason(&open);
    assert!(r.contains("aaa"));
    assert!(r.contains("the bug"));
    assert!(r.contains("fno backlog note --resolve aaa"));
    assert!(r.contains("[+1 more]"));
}
