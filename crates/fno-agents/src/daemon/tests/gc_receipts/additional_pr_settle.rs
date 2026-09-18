//! The additional-PR settle families (tasks 1.1 and 1.2): the three graph
//! rules, the one-GitHub-read stamp, and the pass that applies it.

use super::*;
use super::{
    done_node, no_agents, open_do_row, quiet_transcript, spawn_row, stage_graph, staged_ages,
    staged_graph_home,
};
use crate::gc_sweep::{self, GcSummary};

/// The AC1 specimen: x-spec is done and merged, and its three extras all
/// settle by the graph rules - its own primary, the primary of a merged
/// node, and the primary of an in-review node. The panicking reader in
/// `settle_then_run` proves no read is paid.
fn ac1_graph() -> Vec<Value> {
    vec![
        // x-spec: done, merged, its own primary is PR 2042, with three
        // extras: its own primary (2042), the primary of a merged node
        // (2046, via x-af66), and the primary of an in-review node (2045,
        // via x-holders). No cwd, so the panicking reader is never called.
        json!({
            "id": "x-spec", "status": "done", "merge_status": "merged",
            "pr_number": 2042,
            "pr_url": "https://github.com/o/r/pull/2042",
            "additional_prs": [
                {"number": 2042, "url": "https://github.com/o/r/pull/2042"},
                {"number": 2046, "url": "https://github.com/o/r/pull/2046"},
                {"number": 2045, "url": "https://github.com/o/r/pull/2045"}
            ],
            "sessions": [{"phase": "do", "harness": "claude", "session_id": "sess-spec",
                          "started_at": "2026-09-01T01:00:00Z"}]
        }),
        done_node("x-af66", json!("merged"), json!([]), vec![]),
        // The primaries the extras repeat. 2045 belongs to an in-review
        // node; 2046's holder reads done but unmerged, and rule 3 (another
        // node's primary) settles both.
        json!({
            "id": "x-holders", "status": "in_review", "merge_status": json!(null),
            "pr_number": 2045,
            "pr_url": "https://github.com/o/r/pull/2045",
        }),
        json!({
            "id": "x-af66-hold", "status": "done", "merge_status": json!(null),
            "pr_number": 2046,
            "pr_url": "https://github.com/o/r/pull/2046",
        }),
    ]
}

/// The settle over a staged reader, then the row pass - the real branch of
/// `settle_then_run` with the reader the test owns.
fn settle_staged_then_run(
    home: &AgentsHome,
    emitter: &EventEmitter,
    transcripts: &dyn Fn(&state::RegistryEntry) -> Option<Vec<std::path::PathBuf>>,
    read: &mut dyn FnMut(&str, &str) -> Option<gc_sweep::PrState>,
) -> GcSummary {
    let (settled, refused) = gc_sweep::settle_stale_do_rows_with(home, read);
    let mut summary = gc_sweep::run(
        home,
        emitter,
        0,
        false,
        7,
        &gc_sweep::read_graph_entries,
        transcripts,
        &staged_ages(transcripts),
        &|_| true,
        &|_| crate::daemon::CascadeOutcome::NotApplicable,
        &|_e| crate::daemon::CascadeOutcome::NotApplicable,
        &no_agents,
        &|_| (None, None),
        &|_| None,
    );
    summary.settled_do_rows = settled
        .into_iter()
        .map(|row| (row.node, row.harness, row.session_id))
        .collect();
    summary.settle_refused = refused;
    summary
}

#[test]
fn ac1_the_three_graph_rules_settle_and_no_read_is_paid() {
    let (dir, home) = staged_graph_home();
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let transcripts = tempfile::tempdir().unwrap();
    let quiet = quiet_transcript(transcripts.path(), "a.jsonl", 2 * 3600);
    stage_graph(dir.path(), json!(ac1_graph()));
    state::update_registry(&home.registry_json(), |r| {
        spawn_row(r, "row-spec", "sess-spec");
    })
    .unwrap();

    let summary = settle_then_run(
        &home,
        &emitter,
        false,
        &move |_| Some(vec![quiet.clone()]),
        no_agents(),
    );

    assert_eq!(
        summary.settled_do_rows,
        vec![("x-spec".into(), "claude".into(), "sess-spec".into())]
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
    assert!(summary.retired.iter().any(|(id, _)| id == "row-spec"));
}

#[test]
fn ac1_edge_the_dry_run_reports_the_settle_without_a_hold() {
    let (dir, home) = staged_graph_home();
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let transcripts = tempfile::tempdir().unwrap();
    let quiet = quiet_transcript(transcripts.path(), "a.jsonl", 2 * 3600);
    stage_graph(dir.path(), json!(ac1_graph()));
    state::update_registry(&home.registry_json(), |r| {
        spawn_row(r, "row-spec", "sess-spec");
    })
    .unwrap();

    let summary = settle_then_run(
        &home,
        &emitter,
        true,
        &move |_| Some(vec![quiet.clone()]),
        no_agents(),
    );

    assert_eq!(
        summary.settled_do_rows,
        vec![("x-spec".into(), "claude".into(), "sess-spec".into())]
    );
    assert!(
        summary.kept_open_do_row.is_empty(),
        "{:?}",
        summary.kept_open_do_row
    );
}

/// The AC2/AC3 specimen: a done+merged node with a `cwd`, an open do row,
/// and one extra that no graph rule can settle. `completed_at` rides so the
/// store's status derivation (any write recomputes) keeps the node `done`
/// across the stamp write.
fn held_extra_node(id: &str) -> Value {
    json!({
        "id": id, "title": "Stamp me", "slug": id, "type": "feature",
        "status": "done", "priority": "p2",
        "created_at": "2026-09-11T00:00:00+00:00",
        "completed_at": "2026-09-11T02:00:00+00:00",
        "merge_status": "merged",
        "cwd": "/repo/wt",
        "sessions": [open_do_row("claude", "sess-stamp")],
        "additional_prs": [{"number": 1523}]
    })
}

fn stamp_test_setup(
    id: &str,
) -> (
    tempfile::TempDir,
    AgentsHome,
    EventEmitter,
    std::path::PathBuf,
    tempfile::TempDir,
) {
    let (dir, home) = staged_graph_home();
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let transcripts = tempfile::tempdir().unwrap();
    let quiet = quiet_transcript(transcripts.path(), "b.jsonl", 2 * 3600);
    stage_graph(dir.path(), json!([held_extra_node(id)]));
    state::update_registry(&home.registry_json(), |r| {
        spawn_row(r, "row-stamp", "sess-stamp");
    })
    .unwrap();
    (dir, home, emitter, quiet, transcripts)
}

#[test]
fn ac2_a_merged_answer_stamps_and_settles() {
    let (dir, home, emitter, quiet, _t) = stamp_test_setup("x-stamp");
    let mut read = |path: &str, _cwd: &str| {
        assert_eq!(path, "repos/{owner}/{repo}/pulls/1523");
        Some(gc_sweep::PrState::Merged)
    };
    let summary = settle_staged_then_run(
        &home,
        &emitter,
        &move |_| Some(vec![quiet.clone()]),
        &mut read,
    );

    assert!(
        summary.settle_refused.is_empty(),
        "refused: {:?}",
        summary.settle_refused
    );
    let raw: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("graph.json")).unwrap()).unwrap();
    let extras = &raw["entries"][0]["additional_prs"];
    assert_eq!(extras[0]["merge_status"], json!("merged"));
    assert_eq!(
        summary.settled_do_rows,
        vec![("x-stamp".into(), "claude".into(), "sess-stamp".into())]
    );
    assert!(summary.retired.iter().any(|(id, _)| id == "row-stamp"));
}

#[test]
fn ac3_a_closed_answer_stamps_and_settles() {
    let (dir, home, emitter, quiet, _t) = stamp_test_setup("x-stamp");
    let mut read = |path: &str, _cwd: &str| {
        assert_eq!(path, "repos/{owner}/{repo}/pulls/1523");
        Some(gc_sweep::PrState::Closed)
    };
    let summary = settle_staged_then_run(
        &home,
        &emitter,
        &move |_| Some(vec![quiet.clone()]),
        &mut read,
    );

    assert_eq!(
        summary.settled_do_rows,
        vec![("x-stamp".into(), "claude".into(), "sess-stamp".into())]
    );
    assert!(summary.retired.iter().any(|(id, _)| id == "row-stamp"));
    let raw: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("graph.json")).unwrap()).unwrap();
    let extras = &raw["entries"][0]["additional_prs"];
    assert_eq!(extras[0]["merge_status"], json!("closed"));
}

#[test]
fn ac2_edge_an_open_or_unreadable_answer_holds() {
    for answer in [Some(gc_sweep::PrState::Open), None] {
        let (_dir, home, emitter, quiet, _t) = stamp_test_setup("x-stamp");
        let mut read = |_path: &str, _cwd: &str| answer;
        let summary = settle_staged_then_run(
            &home,
            &emitter,
            &move |_| Some(vec![quiet.clone()]),
            &mut read,
        );

        assert!(summary.settled_do_rows.is_empty());
        assert_eq!(
            summary.kept_open_do_row,
            vec![("row-stamp".to_string(), "x-stamp".to_string())]
        );
        let hold = summary
            .holds
            .iter()
            .find(|hold| hold.id == "row-stamp")
            .expect("the held row carries a hold");
        assert_eq!(hold.detail, "additional_prs: 1 of 1 not recorded merged");
    }
}

#[test]
fn ac4_node_states_read_an_open_count_of_zero() {
    let (dir, home) = staged_graph_home();
    stage_graph(dir.path(), json!(ac1_graph()));
    let states = gc_sweep::read_graph_node_states(&home).unwrap();
    let (_, _, open) = states["x-spec"].clone();
    assert_eq!(open, 0);
}
