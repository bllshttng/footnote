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
        reads: None,
    }
}

fn read_graph(path: &PathBuf) -> Vec<serde_json::Value> {
    // graph.db is the only store; the json file is a frozen mirror under it.
    // Every row a writer landed lives in the store, so the reads go there.
    fno_agents::graph_store::read_rows(path).unwrap()
}

#[test]
fn replace_state_returns_the_view_it_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    write_graph(&graph, &[fixture_node("t-1", "d")]);
    let first = node_state::replace_state(&graph, &ws(&graph, "t-1", "body one")).unwrap();
    assert!(first.replaced.is_none());
    let second = node_state::replace_state(&graph, &ws(&graph, "t-1", "body two")).unwrap();
    let prior = second.replaced.expect("the second write replaced a state");
    assert_eq!(prior.revision, 1);
    assert_eq!(prior.body, "body one");
}

#[test]
fn a_fresh_graph_write_does_not_wait_on_its_own_creation_lock() {
    // x-94e3: the publication seam holds the store lock; opening the store
    // must not re-take the creation lock behind it. Before the fix every
    // replace_state on a fresh graph burned the full 10s timeout, swallowed
    // the failure, and left no graph.db behind.
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    write_graph(&graph, &[fixture_node("t-lock", "d")]);
    let started = std::time::Instant::now();
    node_state::replace_state(&graph, &ws(&graph, "t-lock", "body")).unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "fresh-graph write waited {:?} on its own creation lock",
        elapsed
    );
    assert!(
        dir.path().join("graph.db").exists(),
        "the store was created"
    );
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
            base_version: fno_agents::graph_store::base_version(graph).unwrap(),
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
        reads: None,
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
            None,
            &original,
            None,
            None,
        )
        .unwrap();
    }
    let (_, h) = fno_agents::backlog::note_history::read(&graph, Some("h-1"), 0, 100).unwrap();
    assert_eq!(h, 1, "identical records dedupe to one");
}

#[test]
fn a_state_write_keeps_a_row_another_writer_landed() {
    // Thread one loops replace_state on t-1; thread two
    // appends 30 rows through graph_store::mutate_rows. Every append lands,
    // and t-1's final revision equals thread one's success count.
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    let mut seed: Vec<serde_json::Value> = (0..500)
        .map(|i| fixture_node(&format!("r-{i:04}"), "d"))
        .collect();
    seed.push(fixture_node("t-1", "d"));
    write_graph(&graph, &seed);
    let note_graph = graph.clone();
    let notes = std::thread::spawn(move || {
        let mut ok = 0u64;
        for i in 0..25 {
            let input = StateWriteInput {
                node_id: "t-1".to_string(),
                body: format!("state {i}"),
                if_revision: None,
                source_session_id: None,
                source_harness: None,
                reads: None,
            };
            if node_state::replace_state(&note_graph, &input).is_ok() {
                ok += 1;
            }
            // Yield the flock between writes so the appender thread is not
            // starved past its lock deadline (see the keeper race test).
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        ok
    });
    let append_graph = graph.clone();
    let appends = std::thread::spawn(move || {
        let mut landed = Vec::new();
        for i in 0..30 {
            let id = format!("c-app-{i:04}");
            // Outer retry, the shape cmd_idea uses: mutate_rows' internal 5
            // attempts can exhaust under sustained note-writer contention.
            let mut landed_here = false;
            for _attempt in 0..10 {
                let outcome = fno_agents::graph_store::mutate_rows(
                    &append_graph,
                    std::time::Duration::from_secs(30),
                    None,
                    None,
                    |rows| {
                        rows.push(json!({
                            "id": id,
                            "slug": format!("slug-{id}"),
                            "title": format!("appended {id}"),
                            "type": "feature",
                            "status": "intake",
                            "priority": "p2",
                        }));
                        Ok(true)
                    },
                );
                if outcome.is_ok() {
                    landed_here = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            assert!(landed_here, "append {id} never landed in 10 outer tries");
            landed.push(id);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        landed
    });
    let ok = notes.join().unwrap();
    let landed = appends.join().unwrap();
    let final_ids: Vec<String> = read_graph(&graph)
        .iter()
        .filter_map(|r| r["id"].as_str().map(str::to_string))
        .collect();
    for id in &landed {
        assert!(
            final_ids.contains(id),
            "append {id} landed but is missing from the final graph"
        );
    }
    let row = read_graph(&graph)
        .into_iter()
        .find(|r| r["id"] == json!("t-1"))
        .unwrap();
    let revision = row[node_state::STATE_KEY]["revision"].as_u64().unwrap();
    assert_eq!(
        revision, ok,
        "t-1 answered ok {ok} times but its final revision is {revision}"
    );
}
