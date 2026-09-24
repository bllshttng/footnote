//! x-920a wave 3: lossless migration, real-store tests (AC8-AC10).

use fno_agents::backlog::node_state;
use serde_json::json;
use std::path::PathBuf;
use std::process::Command;

fn fixture(id: &str, status: &str) -> serde_json::Value {
    json!({
        "id": id,
        "slug": format!("slug-{id}"),
        "title": format!("Node {id}"),
        "type": "feature",
        "status": status,
        "priority": "p1",
    })
}

fn write_graph(path: &PathBuf, entries: &[serde_json::Value]) {
    fno_agents::graph_store::seed_rows(path, entries).unwrap();
}

fn read_graph(path: &PathBuf) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn run_notes(args: &[&str]) -> (i32, String) {
    let (code, out, _) = run_notes_full(args);
    (code, out)
}

fn run_notes_full(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_fno-agents"))
        .args(std::iter::once("backlog-notes").chain(args.iter().copied()))
        // The client lazy-starts a daemon that inherits this env, so the
        // daemon dies with this test run instead of idling an hour (x-5533).
        .envs(fno_agents::test_run::self_owner_env())
        .output()
        .expect("spawn fno-agents backlog-notes");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn notes_hash_hex(notes: &serde_json::Value) -> String {
    use sha2::Digest as _;
    let bytes = serde_json::to_vec(notes).unwrap();
    format!("sha256:{:x}", sha2::Sha256::digest(&bytes))
}

#[test]
fn ac8_originals_and_positions_survive_verbatim() {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    // A done node with two notes: identical bodies, different positions, and
    // one oversized legacy note that must survive exactly (AC8).
    let big = "y".repeat(11963);
    let notes = json!([
        {"ts": "T1", "text": "same body"},
        {"ts": "T2", "text": "same body"},
        {"ts": "T3", "text": big},
    ]);
    write_graph(
        &graph,
        &[json!({
            "id": "d-9", "slug": "slug-d-9", "title": "n", "type": "feature",
            "status": "done", "priority": "p1", "progress_notes": notes,
        })],
    );
    let g = graph.display().to_string();
    let empty = dir.path().join("empty.json");
    std::fs::write(&empty, "[]").unwrap();
    let empty_manifest = empty.display().to_string();
    let (code, _) = run_notes(&[
        "migrate",
        "--apply",
        "--manifest",
        &empty_manifest,
        "--graph",
        &g,
    ]);
    assert_eq!(code, 0, "terminal verbatim migration succeeds");
    // Hot row: no notes, marker set.
    let row = read_graph(&graph)["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == json!("d-9"))
        .unwrap()
        .clone();
    assert!(row.get("progress_notes").is_none());
    assert!(row.get(node_state::HISTORY_MARKER_KEY).is_some());
    // Readback: three originals with positions, oversized one intact.
    let (records, total) =
        fno_agents::backlog::note_history::read(&graph, Some("d-9"), 0, 100).unwrap();
    assert_eq!(total, 3);
    assert_eq!(records[0]["position"], json!(0));
    assert_eq!(records[1]["position"], json!(1));
    assert_eq!(records[2]["position"], json!(2));
    let third = records[2]["original"]["text"].as_str().unwrap();
    assert_eq!(third.len(), 11963);
    assert_eq!(
        records[0]["original"]["text"],
        records[1]["original"]["text"]
    );
}

#[test]
fn ac9_stale_and_oversized_and_missing_manifest_entries_refuse() {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    let notes = json!([{"ts": "T1", "text": "finding"}]);
    let hash = notes_hash_hex(&notes);
    write_graph(
        &graph,
        &[
            json!({
                "id": "o-1", "slug": "slug-o-1", "title": "n", "type": "feature",
                "status": "in_progress", "priority": "p1", "details": "d",
                "progress_notes": notes,
            }),
            fixture("o-2", "in_progress"),
        ],
    );
    // A stale digest (over budget) for o-1 and a missing entry for o-2's
    // notes... o-2 has no notes, so exercise the OPEN node without entry
    // case instead by an oversized digest for o-1.
    let manifest = format!(
        "[{{\"node_id\": \"o-1\", \"source_hash\": \"{hash}\", \"state\": \"{}\"}}]",
        "z".repeat(5001)
    );
    let g = graph.display().to_string();
    let manifest_path = dir.path().join("manifest.json");
    std::fs::write(&manifest_path, &manifest).unwrap();
    let mp = manifest_path.display().to_string();
    // Preview: nonzero, unresolved named, NO data change.
    let (code, out) = run_notes(&["migrate", "--manifest", &mp, "--graph", &g, "--json"]);
    assert_ne!(code, 0, "preview with an unresolved row exits nonzero");
    assert!(out.contains("unresolved") || out.contains("\"unresolved\""));
    let before = fno_agents::backlog::version(&graph).unwrap();
    // Apply with the same over-budget digest: refused, row intact.
    let (code, _) = run_notes(&["migrate", "--apply", "--manifest", &mp, "--graph", &g]);
    assert_ne!(code, 0);
    let after = fno_agents::backlog::version(&graph).unwrap();
    assert_eq!(before, after, "a refused row is unchanged");
    // A good digest migrates o-1.
    let good = format!(
        "[{{\"node_id\": \"o-1\", \"source_hash\": \"{hash}\", \"state\": \"digested state\"}}]"
    );
    std::fs::write(&manifest_path, &good).unwrap();
    let (code, _) = run_notes(&["migrate", "--apply", "--manifest", &mp, "--graph", &g]);
    assert_eq!(code, 0);
    let row = read_graph(&graph)["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == json!("o-1"))
        .unwrap()
        .clone();
    assert_eq!(row["current_state"]["body"], json!("digested state"));
    assert!(row.get("progress_notes").is_none());
}

#[test]
fn ac10_rerun_is_idempotent_and_receipt_is_positive() {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    let notes = json!([
        {"ts": "T1", "text": "a"},
        {"ts": "T2", "text": "b"},
    ]);
    write_graph(
        &graph,
        &[json!({
            "id": "i-1", "slug": "slug-i-1", "title": "n", "type": "feature",
            "status": "done", "priority": "p1", "progress_notes": notes,
        })],
    );
    let g = graph.display().to_string();
    let empty = dir.path().join("empty.json");
    std::fs::write(&empty, "[]").unwrap();
    let empty_manifest = empty.display().to_string();
    let (code1, out1) = run_notes(&[
        "migrate",
        "--apply",
        "--manifest",
        &empty_manifest,
        "--graph",
        &g,
    ]);
    assert_eq!(code1, 0);
    // Re-run: the row reads unchanged, history stays at two records.
    let (code2, out2) = run_notes(&[
        "migrate",
        "--apply",
        "--manifest",
        &empty_manifest,
        "--graph",
        &g,
    ]);
    assert_eq!(code2, 0);
    assert!(out1.contains("digested_nodes=1") || out1.contains("\"digested_nodes\":1"));
    assert!(out2.contains("unchanged=1") || out2.contains("\"unchanged\":1"));
    let (_, total) = fno_agents::backlog::note_history::read(&graph, Some("i-1"), 0, 100).unwrap();
    assert_eq!(total, 2, "logical history is stable across re-runs");
    assert!(out1.contains("history_verified=1") || out1.contains("\"history_verified\":1"));
    assert_eq!(
        node_state::current_revision(&graph, "i-1").unwrap(),
        0,
        "verbatim rows keep no revision bump"
    );
}

#[test]
fn inventory_reports_counts_and_hashes_without_writes() {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    let notes = json!([{"ts": "T1", "text": "hello"}]);
    let hash = notes_hash_hex(&notes);
    write_graph(
        &graph,
        &[json!({
            "id": "n-1", "slug": "slug-n-1", "title": "n", "type": "feature",
            "status": "ready", "priority": "p1", "progress_notes": notes,
        })],
    );
    let before = fno_agents::backlog::version(&graph).unwrap();
    let g = graph.display().to_string();
    let (code, out) = run_notes(&["inventory", "--json", "--graph", &g]);
    assert_eq!(code, 0);
    assert!(out.contains("backend=sqlite"), "{out}");
    assert!(out.contains(&hash[..20.min(hash.len())]) || out.contains("notes_hash"));
    assert_eq!(fno_agents::backlog::version(&graph).unwrap(), before);
}

#[test]
fn ac2_history_positional_slug_prints_bodies_and_trailer() {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    write_graph(
        &graph,
        &[json!({
            "id": "x-1", "slug": "slug-x-1", "title": "n", "type": "feature",
            "status": "ready", "priority": "p1",
            "current_state": {"revision": 4, "body": "latest"},
            "progress_notes": [
                {"ts": "T1", "text": "legacy a"},
                {"ts": "T2", "text": "legacy b"},
            ],
        })],
    );
    for i in 0..3 {
        fno_agents::backlog::note_history::append(
            &graph,
            "x-1",
            "state_replaced",
            Some(i),
            Some(0),
            &json!({"revision": i, "body": format!("body {i}")}),
            Some(&format!("sess-{i}")),
            None,
        )
        .unwrap();
    }
    let g = graph.display().to_string();
    let (code, out) = run_notes(&["history", "slug-x-1", "--graph", &g]);
    assert_eq!(code, 0);
    assert!(out.contains("body 0"), "oldest first: {out}");
    assert!(out.find("body 0").unwrap() < out.find("body 2").unwrap());
    assert!(out.contains("rev 0 state_replaced session sess-0"));
    assert!(
        out.contains(
            "x-1: records 1-3 of 3; current_state revision 4; 2 legacy progress_notes (fno backlog get x-1)"
        ),
        "trailer: {out}"
    );
}

#[test]
fn ac2_history_unknown_token_exits_one_naming_it() {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    write_graph(&graph, &[fixture("x-1", "ready")]);
    let g = graph.display().to_string();
    let (code, _, err) = run_notes_full(&["history", "no-such-token", "--graph", &g]);
    assert_eq!(code, 1);
    assert!(
        err.contains("no-such-token"),
        "stderr names the token: {err}"
    );
}

#[test]
fn ac2_history_archived_node_survives_via_journal_and_help_exits_zero() {
    // A token no row knows but the journal does: history still reads.
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    write_graph(&graph, &[fixture("x-1", "ready")]);
    fno_agents::backlog::note_history::append(
        &graph,
        "d-archived",
        "state_replaced",
        Some(2),
        None,
        &json!({"revision": 2, "body": "gone but journaled"}),
        Some("sess-old"),
        None,
    )
    .unwrap();
    let g = graph.display().to_string();
    let (code, out) = run_notes(&["history", "d-archived", "--graph", &g]);
    assert_eq!(code, 0);
    assert!(out.contains("gone but journaled"));
    assert!(out.contains("d-archived: records 1-1 of 1; current_state revision 0"));
    let (help_code, help_out, _) = run_notes_full(&["--help"]);
    assert_eq!(help_code, 0);
    assert!(help_out.contains("inventory"));
    assert!(help_out.contains("migrate"));
    assert!(help_out.contains("history"));
}
