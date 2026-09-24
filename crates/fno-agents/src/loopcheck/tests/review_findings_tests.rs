use super::*;

#[test]
fn review_finding_open_then_resolved_clears() {
    // AC2-HP: an open review_finding gates; an explicit resolve clears it.
    let tmp = tempfile::tempdir().unwrap();
    let open = write_events(
        tmp.path(),
        &[
            r#"{"ts":"t1","type":"review_finding","source":"observer","data":{"finding_id":"f1","node":"x-1","text":"off-by-one in the loop\nsecond line"}}"#,
        ],
    );
    let (findings, malformed) = open_review_findings(&open, "x-1");
    assert_eq!(malformed, 0);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].id, "f1");
    assert_eq!(findings[0].first_line, "off-by-one in the loop"); // first line only

    // resolve clears it (node-scoped, only an explicit resolve).
    let resolved = write_events(
        tmp.path(),
        &[
            r#"{"ts":"t1","type":"review_finding","source":"observer","data":{"finding_id":"f1","node":"x-1","text":"off-by-one"}}"#,
            r#"{"ts":"t2","type":"review_finding_resolved","source":"observer","data":{"finding_id":"f1"}}"#,
        ],
    );
    assert!(open_review_findings(&resolved, "x-1").0.is_empty());
}

#[test]
fn review_finding_is_node_scoped() {
    // A finding for a different node must not gate this node.
    let tmp = tempfile::tempdir().unwrap();
    let p = write_events(
        tmp.path(),
        &[
            r#"{"ts":"t","type":"review_finding","source":"observer","data":{"finding_id":"f1","node":"x-OTHER","text":"not mine"}}"#,
        ],
    );
    assert!(open_review_findings(&p, "x-mine").0.is_empty());
    assert_eq!(open_review_findings(&p, "x-OTHER").0.len(), 1);
}

#[test]
fn review_finding_malformed_notices_not_blocks() {
    // AC3-FR: a structurally-unparseable review_finding line does NOT block
    // (no open finding), but is counted for the audit notice. A review_finding
    // missing its id is likewise a malformed notice, never a gating finding.
    let tmp = tempfile::tempdir().unwrap();
    // A truncated (unparseable) line that still carries the review_finding marker.
    let truncated = r#"{"ts":"t","type":"review_finding","data":{"finding_id":"f1"#;
    let id_less = r#"{"ts":"t","type":"review_finding","source":"observer","data":{"node":"x-1","text":"no id"}}"#;
    let good = r#"{"ts":"t","type":"review_finding","source":"observer","data":{"finding_id":"good","node":"x-1","text":"real one"}}"#;
    let p = write_events(tmp.path(), &[truncated, id_less, good]);
    let (findings, malformed) = open_review_findings(&p, "x-1");
    assert_eq!(findings.len(), 1, "only the well-formed finding gates");
    assert_eq!(findings[0].id, "good");
    assert_eq!(
        malformed, 2,
        "the truncated line + the id-less line are noticed"
    );
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
    let r = build_findings_block_reason(&open, 1);
    assert!(r.contains("aaa"));
    assert!(r.contains("the bug"));
    assert!(r.contains("fno backlog annotate resolve aaa"));
    assert!(r.contains("[+1 more]"));
    assert!(r.contains("1 malformed"));
}
