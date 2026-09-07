//! The settle of stale open do rows on settled nodes (x-8739): fill
//! `ended_at`, KEEP the row. Every graph-facing test here runs against a
//! REAL graph file in a tmp state root, never the injected `GraphRead` seam:
//! the seam models an open row by its presence in a map, so it can prove a
//! keep but it cannot tell a fill from a delete, which is the whole hazard.
//! The fill-and-keep assertions are the load-bearing ones; a settle that
//! removed the row would pass every count.
use super::*;

use super::gc_receipts::quiet_transcript;
use crate::gc_sweep::{self, GcSummary};

/// A home whose graph path (`home.root().parent()/graph.json`) lands INSIDE
/// the test's tmpdir: the root is a subdir of it. `tmp_home` makes the root
/// the tmpdir itself, so its graph path would be the shared temp dir.
fn staged_graph_home() -> (tempfile::TempDir, AgentsHome) {
    let dir = tempfile::tempdir().unwrap();
    let home = AgentsHome::at(dir.path().join("agents"));
    home.ensure_root().unwrap();
    (dir, home)
}

/// Stage a real graph file at the state root.
fn stage_graph(dir: &std::path::Path, entries: Value) {
    std::fs::write(
        dir.join("graph.json"),
        serde_json::to_vec(&json!({ "entries": entries })).unwrap(),
    )
    .unwrap();
}

/// The settled-node shape: done, GitHub-confirmed merged, no additional PR.
fn done_node(id: &str, merge_status: Value, aprs: Value, sessions: Vec<Value>) -> Value {
    json!({
        "id": id,
        "status": "done",
        "completed_at": "2026-09-01T00:00:00Z",
        "merge_status": merge_status,
        "additional_prs": aprs,
        "sessions": sessions,
    })
}

/// One open do row.
fn open_do_row(harness: &str, sid: &str) -> Value {
    json!({
        "phase": "do",
        "harness": harness,
        "session_id": sid,
        "started_at": "2026-09-01T01:00:00Z",
    })
}

/// The shipped shell's order against a REAL graph file: settle first, then
/// the row pass over the production graph read. Only the transcript/stop/
/// tree seams are staged (gc::gc_sweep resolves those from live stores a
/// tmp home does not have); the settle -> run -> read ordering under test is
/// the wiring gc.rs ships.
fn settle_then_run(
    home: &AgentsHome,
    emitter: &EventEmitter,
    dry_run: bool,
    transcripts: &dyn Fn(&state::RegistryEntry) -> Option<Vec<std::path::PathBuf>>,
) -> GcSummary {
    if dry_run {
        let planned = gc_sweep::plan_stale_do_rows(home);
        let mut summary = gc_sweep::run(
            home,
            emitter,
            0,
            true,
            0,
            &|h| gc_sweep::read_graph_entries(h).map(|g| gc_sweep::without_settled(g, &planned)),
            transcripts,
            &|_| true,
            &|_| (None, None),
            &|_| {},
        );
        summary.settled_do_rows = planned
            .into_iter()
            .map(|row| (row.node, row.harness, row.session_id))
            .collect();
        summary
    } else {
        let (settled, refused) = gc_sweep::settle_stale_do_rows(home);
        let mut summary = gc_sweep::run(
            home,
            emitter,
            0,
            false,
            7,
            &gc_sweep::read_graph_entries,
            transcripts,
            &|_| true,
            &|_| (None, None),
            &|_| {},
        );
        summary.settled_do_rows = settled
            .into_iter()
            .map(|row| (row.node, row.harness, row.session_id))
            .collect();
        summary.settle_refused = refused;
        summary
    }
}

/// A registry row named on `sid`, spawned by fno, transcript-probeable.
fn spawn_row(reg: &mut state::Registry, name: &str, sid: &str) {
    let mut e = ask_row(name, None);
    e.harness = Some("claude".into());
    e.harness_session_id = Some(sid.into());
    e.origin = Some("spawn".into());
    reg.entries.push(e);
}

/// AC3-HP: the settle FILLS `ended_at` and KEEPS the row - the sessions
/// array keeps its length, the row keeps its phase, harness and session id,
/// and the stamp names the sweep as the ender. The session then retires
/// through the ordinary gates.
#[test]
fn a_settled_nodes_open_do_row_is_filled_and_kept() {
    let (dir, home) = staged_graph_home();
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let transcripts = tempfile::tempdir().unwrap();
    let quiet = quiet_transcript(transcripts.path(), "a.jsonl", 2 * 3600);
    stage_graph(
        dir.path(),
        json!([done_node(
            "N1",
            json!("merged"),
            json!([]),
            vec![open_do_row("claude", "sess-a")],
        )]),
    );
    state::update_registry(&home.registry_json(), |r| {
        spawn_row(r, "row-a", "sess-a");
    })
    .unwrap();

    let summary = settle_then_run(&home, &emitter, false, &move |_| Some(vec![quiet.clone()]));

    assert_eq!(
        summary.settled_do_rows,
        vec![("N1".into(), "claude".into(), "sess-a".into())]
    );
    assert!(
        summary.settle_refused.is_empty(),
        "{:?}",
        summary.settle_refused
    );
    assert!(
        summary.kept_open_do_row.is_empty(),
        "{:?}",
        summary.kept_open_do_row
    );
    assert_eq!(
        summary.retired,
        vec![("row-a".to_string(), "every named node done: N1".to_string())]
    );
    // THE assertion: the file still holds the row, now closed, never removed.
    let raw: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("graph.json")).unwrap()).unwrap();
    let entry = &raw["entries"][0];
    let sessions = entry["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    let row = &sessions[0];
    assert_eq!(row["phase"], json!("do"));
    assert_eq!(row["session_id"], json!("sess-a"));
    assert_eq!(row["harness"], json!("claude"));
    assert_eq!(row["started_at"], json!("2026-09-01T01:00:00Z"));
    assert!(!row["ended_at"].as_str().unwrap_or_default().is_empty());
    assert_eq!(row["ended_by"], json!("reap-sweep"));
    assert_eq!(entry["status"], json!("done"));
    assert_eq!(entry["merge_status"], json!("merged"));
}

/// AC2-EDGE, unmerged arm: a done node whose `merge_status` never resolved
/// from `gh` still holds its row - nothing settled, nothing stamped, the
/// session kept under the existing reason.
#[test]
fn a_done_but_unmerged_node_still_holds_its_row() {
    let (dir, home) = staged_graph_home();
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let transcripts = tempfile::tempdir().unwrap();
    let quiet = quiet_transcript(transcripts.path(), "b.jsonl", 2 * 3600);
    stage_graph(
        dir.path(),
        json!([done_node(
            "N2",
            Value::Null,
            json!([]),
            vec![open_do_row("claude", "sess-b")],
        )]),
    );
    state::update_registry(&home.registry_json(), |r| {
        spawn_row(r, "row-b", "sess-b");
    })
    .unwrap();

    let summary = settle_then_run(&home, &emitter, false, &move |_| Some(vec![quiet.clone()]));

    assert!(summary.settled_do_rows.is_empty());
    assert!(summary.retired.is_empty());
    assert_eq!(
        summary.kept_open_do_row,
        vec![("row-b".to_string(), "N2".to_string())]
    );
    let raw: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("graph.json")).unwrap()).unwrap();
    let row = &raw["entries"][0]["sessions"][0];
    assert!(row.get("ended_at").is_none(), "{row}");
    let rendered = crate::reap_render::render_reap(&summary, false, false);
    assert!(
        rendered.contains("open do row on done node: N2"),
        "{rendered}"
    );
}

/// AC2-EDGE, additional-PR arm: one open additional PR holds the row, even
/// done and merged - the graph records no per-entry merge state for it.
#[test]
fn an_open_additional_pr_still_holds_its_row() {
    let (dir, home) = staged_graph_home();
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let transcripts = tempfile::tempdir().unwrap();
    let quiet = quiet_transcript(transcripts.path(), "c.jsonl", 2 * 3600);
    stage_graph(
        dir.path(),
        json!([done_node(
            "N3",
            json!("merged"),
            json!([{ "number": 1488 }]),
            vec![open_do_row("claude", "sess-c")],
        )]),
    );
    state::update_registry(&home.registry_json(), |r| {
        spawn_row(r, "row-c", "sess-c");
    })
    .unwrap();

    let summary = settle_then_run(&home, &emitter, false, &move |_| Some(vec![quiet.clone()]));

    assert!(summary.settled_do_rows.is_empty());
    assert!(summary.retired.is_empty());
    assert_eq!(
        summary.kept_open_do_row,
        vec![("row-c".to_string(), "N3".to_string())]
    );
    let raw: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("graph.json")).unwrap()).unwrap();
    assert!(raw["entries"][0]["sessions"][0].get("ended_at").is_none());
}

/// The measured live split, staged: of the reaper's kept rows, 15 nodes (17
/// open do rows, two nodes holding two each) are done AND merged AND carry
/// no additional PR, and 3 correctly fail - one unmerged, two with an open
/// additional PR. A one-directional test cannot tell the fix from the
/// regression; both directions are asserted by name.
#[test]
fn the_live_eighteen_split_fifteen_and_three() {
    let (dir, home) = staged_graph_home();
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let transcripts = tempfile::tempdir().unwrap();
    let mut entries = Vec::new();
    let mut sids: Vec<String> = Vec::new();
    for i in 0..15u32 {
        let node = format!("N{i:02}");
        let rows = if i < 2 {
            // Two nodes hold two open do rows each (x-3a6f, x-cdf6 live).
            vec![
                open_do_row("claude", &format!("sess-{node}-a")),
                open_do_row("codex", &format!("sess-{node}-b")),
            ]
        } else {
            vec![open_do_row("claude", &format!("sess-{node}"))]
        };
        for row in &rows {
            sids.push(row["session_id"].as_str().unwrap().to_string());
        }
        entries.push(done_node(&node, json!("merged"), json!([]), rows));
    }
    entries.push(done_node(
        "Nu",
        Value::Null,
        json!([]),
        vec![open_do_row("claude", "sess-u")],
    ));
    entries.push(done_node(
        "Np1",
        json!("merged"),
        json!([{ "number": 1488 }]),
        vec![open_do_row("claude", "sess-p1")],
    ));
    entries.push(done_node(
        "Np2",
        json!("merged"),
        json!([{ "number": 1522 }]),
        vec![open_do_row("claude", "sess-p2")],
    ));
    stage_graph(dir.path(), json!(entries));
    state::update_registry(&home.registry_json(), |r| {
        for sid in &sids {
            spawn_row(r, &format!("row-{sid}"), sid);
        }
        spawn_row(r, "row-sess-u", "sess-u");
        spawn_row(r, "row-sess-p1", "sess-p1");
        spawn_row(r, "row-sess-p2", "sess-p2");
    })
    .unwrap();

    let summary = settle_then_run(&home, &emitter, false, &move |e| {
        Some(vec![quiet_transcript(
            transcripts.path(),
            &format!("{}.jsonl", e.harness_session_id.as_deref().unwrap_or("x")),
            2 * 3600,
        )])
    });

    assert_eq!(
        summary.settled_do_rows.len(),
        17,
        "{:?}",
        summary.settled_do_rows
    );
    let nodes: std::collections::BTreeSet<&String> =
        summary.settled_do_rows.iter().map(|(n, _, _)| n).collect();
    assert_eq!(nodes.len(), 15);
    assert_eq!(summary.retired.len(), 17, "{:?}", summary.retired);
    let held: Vec<&String> = summary.kept_open_do_row.iter().map(|(_, n)| n).collect();
    assert_eq!(held.len(), 3);
    for node in ["Nu", "Np1", "Np2"] {
        assert!(held.contains(&&node.to_string()), "held: {held:?}");
    }
    let raw: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("graph.json")).unwrap()).unwrap();
    for entry in raw["entries"].as_array().unwrap() {
        let node = entry["id"].as_str().unwrap();
        for row in entry["sessions"].as_array().unwrap() {
            let stamped = row.get("ended_at").is_some();
            if ["Nu", "Np1", "Np2"].contains(&node) {
                assert!(!stamped, "{node} must stay open: {row}");
            } else {
                assert!(stamped, "{node} must be settled: {row}");
                assert_eq!(row["ended_by"], json!("reap-sweep"));
            }
        }
    }
}

/// AC4-EDGE: the rehearsal names the settle and touches nothing - the graph
/// file is byte-identical, the row still open on disk, and the row pass
/// reads it as would-retire.
#[test]
fn a_dry_run_settles_nothing_on_disk() {
    let (dir, home) = staged_graph_home();
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let transcripts = tempfile::tempdir().unwrap();
    let quiet = quiet_transcript(transcripts.path(), "d.jsonl", 2 * 3600);
    stage_graph(
        dir.path(),
        json!([done_node(
            "N4",
            json!("merged"),
            json!([]),
            vec![open_do_row("claude", "sess-d")],
        )]),
    );
    state::update_registry(&home.registry_json(), |r| {
        spawn_row(r, "row-d", "sess-d");
    })
    .unwrap();
    let before = std::fs::read(dir.path().join("graph.json")).unwrap();

    let summary = settle_then_run(&home, &emitter, true, &move |_| Some(vec![quiet.clone()]));

    assert_eq!(
        summary.settled_do_rows,
        vec![("N4".into(), "claude".into(), "sess-d".into())]
    );
    assert!(summary.kept_open_do_row.is_empty());
    let after = std::fs::read(dir.path().join("graph.json")).unwrap();
    assert_eq!(before, after, "a dry run wrote the graph");
    let raw: Value = serde_json::from_slice(&after).unwrap();
    assert!(raw["entries"][0]["sessions"][0].get("ended_at").is_none());
    assert!(
        !summary.retired.is_empty(),
        "the row would retire: {summary:?}"
    );
}

/// AC1-EDGE: an unidentified row (no harness) is not an open do row, so it
/// is never counted stale and never settled.
#[test]
fn an_unidentified_do_row_is_not_open() {
    let row = json!({
        "phase": "do",
        "harness": "",
        "session_id": "sess-x",
        "started_at": "2026-09-01T01:00:00Z",
    });
    assert!(!crate::graph_store::is_open_do_row(&row));
    let entries = vec![done_node(
        "N5",
        json!("merged"),
        json!([]),
        vec![row, open_do_row("claude", "sess-y")],
    )];
    let stale = gc_sweep::stale_open_do_rows(&entries);
    assert_eq!(
        stale,
        vec![gc_sweep::StaleDoRow {
            node: "N5".into(),
            harness: "claude".into(),
            session_id: "sess-y".into(),
        }]
    );
}

/// AC3-EDGE: a settle that cannot read the graph is NAMED, never silent,
/// and the settle writes nothing. The write arm rides the same refusal
/// mapping (locked_mutate's Conflict / LockTimeout).
#[test]
fn a_settle_that_cannot_read_is_named_and_changes_nothing() {
    let (dir, home) = staged_graph_home();
    stage_graph(
        dir.path(),
        json!([done_node(
            "N6",
            json!("merged"),
            json!([]),
            vec![open_do_row("claude", "sess-f")],
        )]),
    );
    // Corrupt the file: a failed read is a refusal, never a write.
    std::fs::write(dir.path().join("graph.json"), b"{not json").unwrap();

    let (settled, refused) = gc_sweep::settle_stale_do_rows(&home);

    assert!(settled.is_empty());
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert!(refused[0].1.contains("graph unreadable"), "{refused:?}");
}

/// The shipped dry-run shell itself (not the staged replica) plans the
/// settle, subtracts it from the keep, and writes nothing.
#[test]
fn the_shipped_dry_run_shell_plans_the_settle() {
    let (dir, home) = staged_graph_home();
    stage_graph(
        dir.path(),
        json!([done_node(
            "N7",
            json!("merged"),
            json!([]),
            vec![open_do_row("claude", "sess-g")],
        )]),
    );
    let before = std::fs::read(dir.path().join("graph.json")).unwrap();

    let summary = crate::gc::gc_sweep_dry_run(&home, 0);

    assert_eq!(
        summary.settled_do_rows,
        vec![("N7".into(), "claude".into(), "sess-g".into())]
    );
    assert_eq!(
        before,
        std::fs::read(dir.path().join("graph.json")).unwrap()
    );
}
