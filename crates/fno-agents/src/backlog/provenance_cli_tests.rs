use super::*;
use crate::graph_store;

fn seed(graph: &Path) {
    let rows = json!([
        {"id": "x-aa11", "slug": "a", "title": "A", "type": "feature", "status": "idea",
         "priority": "p2", "domain": "code", "created_at": "2026-10-09T00:00:00+00:00",
         "sessions": [{"phase": "do", "harness": "claude", "session_id": "dfd94113-0000-0000-0000-000000000001"}]},
        {"id": "x-bb22", "slug": "b", "title": "B", "type": "feature", "status": "idea",
         "priority": "p2", "domain": "code", "created_at": "2026-10-09T00:00:00+00:00",
         "spawned_by_session": "born-parent",
         "sessions": [{"phase": "do", "harness": "claude", "session_id": "529383fe-0000-0000-0000-000000000002"}]},
        {"id": "x-cc33", "slug": "c", "title": "C", "type": "feature", "status": "idea",
         "priority": "p2", "domain": "code", "created_at": "2026-10-09T00:00:00+00:00",
         "source_session_id": "707aab13-0000-0000-0000-00000000000a", "source_harness": "claude",
         "sessions": [{"phase": "do", "harness": "claude", "session_id": "1e4b7675-0000-0000-0000-000000000003"}]},
    ]);
    graph_store::seed_rows(graph, rows.as_array().unwrap()).unwrap();
}

const MAP: &str = "\
| launched (UTC) | lead session | worker name | worker session |
|---|---|---|---|
| 2026-10-08T20:46:40Z | 707aab13 | t-x-bb22-opus | 529383fe |
| 2026-10-08T20:23:10Z | 707aab13 | t-x-aa11-sonnet | dfd94113 |
| 2026-10-08T21:00:00Z | 0000dead | t-gone | 1e4b7675 |
| 2026-10-08T21:10:00Z | 707aab13 | t-none | c922c004 |
";

#[test]
fn a_dry_run_reports_every_row_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    seed(&graph);
    let map = dir.path().join("map.md");
    std::fs::write(&map, MAP).unwrap();
    let journal = dir.path().join("backfill.jsonl");
    let lines = backfill(&graph, &map, false, &journal).unwrap();
    assert!(
        lines[0].starts_with("would-write\tt-x-aa11-sonnet"),
        "{lines:?}"
    );
    assert!(lines[1].starts_with("kept\tt-x-bb22-opus") && lines[1].contains("born-parent"));
    assert!(lines[2].starts_with("skip\tt-gone") && lines[2].contains("none"));
    assert!(lines[3].starts_with("skip\tt-none") && lines[3].contains("no single node"));
    let back = graph_store::read_rows(&graph).unwrap();
    assert!(back.iter().find(|r| r["id"] == "x-aa11").unwrap()["spawned_by_session"].is_null());
    assert!(!journal.exists());
}

#[test]
fn apply_writes_empty_edges_once_and_journals_each_write() {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    seed(&graph);
    let map = dir.path().join("map.md");
    std::fs::write(&map, MAP).unwrap();
    let journal = dir.path().join("backfill.jsonl");
    backfill(&graph, &map, true, &journal).unwrap();
    let back = graph_store::read_rows(&graph).unwrap();
    let row = |id: &str| back.iter().find(|r| r["id"] == id).unwrap().clone();
    assert_eq!(
        row("x-aa11")["spawned_by_session"],
        "707aab13-0000-0000-0000-00000000000a"
    );
    assert_eq!(row("x-bb22")["spawned_by_session"], "born-parent");
    assert_eq!(
        std::fs::read_to_string(&journal).unwrap().lines().count(),
        1
    );
    // A second apply finds the edge and keeps it.
    let again = backfill(&graph, &map, true, &journal).unwrap();
    assert!(again[0].starts_with("kept\tt-x-aa11-sonnet"), "{again:?}");
    assert_eq!(
        std::fs::read_to_string(&journal).unwrap().lines().count(),
        1
    );
}

#[test]
fn a_lead_prefix_with_two_sessions_is_skipped_as_ambiguous() {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    let rows = json!([
        {"id": "x-aa11", "slug": "a", "title": "A", "type": "feature", "status": "idea",
         "priority": "p2", "domain": "code", "created_at": "2026-10-09T00:00:00+00:00",
         "source_session_id": "abcd0001-0000-0000-0000-000000000000",
         "sessions": [{"phase": "do", "harness": "claude", "session_id": "dfd94113-0000-0000-0000-000000000001"}]},
        {"id": "x-bb22", "slug": "b", "title": "B", "type": "feature", "status": "idea",
         "priority": "p2", "domain": "code", "created_at": "2026-10-09T00:00:00+00:00",
         "source_session_id": "abcd0002-0000-0000-0000-000000000000"},
    ]);
    graph_store::seed_rows(&graph, rows.as_array().unwrap()).unwrap();
    let map = dir.path().join("map.md");
    std::fs::write(&map, "| 2026-10-08T20:23:10Z | abcd | w | dfd94113 |\n").unwrap();
    let lines = backfill(&graph, &map, true, &dir.path().join("j")).unwrap();
    assert!(
        lines[0].starts_with("skip\tw") && lines[0].contains("ambiguous (2 matches)"),
        "{lines:?}"
    );
    let back = graph_store::read_rows(&graph).unwrap();
    assert!(back.iter().find(|r| r["id"] == "x-aa11").unwrap()["spawned_by_session"].is_null());
}
