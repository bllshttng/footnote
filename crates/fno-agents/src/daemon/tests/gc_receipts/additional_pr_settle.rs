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
            "sessions": [{"phase": "execute", "harness": "claude", "session_id": "sess-spec",
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
    let rows = crate::graph_store::read_rows(&dir.path().join("graph.json")).unwrap();
    let extras = &rows[0]["additional_prs"];
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
    let rows = crate::graph_store::read_rows(&dir.path().join("graph.json")).unwrap();
    let extras = &rows[0]["additional_prs"];
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

// The primary-stamp family: the sweep records a done node's out-of-band
// merged primary PR, so its dead do rows settle, and the dry run rehearses
// the same outcome.

/// The AC1 specimen: a done node whose own merge is unrecorded. Its PR
/// merged out of band, so the Python writers left `merge_status` null and
/// the settle's gate skips the node - the defect this family pins.
fn held_primary_node(id: &str) -> Value {
    json!({
        "id": id, "title": "Stamp my primary", "slug": id, "type": "feature",
        "status": "done", "priority": "p2",
        "created_at": "2026-09-11T00:00:00+00:00",
        "completed_at": "2026-09-11T02:00:00+00:00",
        "merge_status": json!(null),
        "pr_number": 2180,
        "pr_url": "https://github.com/o/r/pull/2180",
        "cwd": "/repo/wt",
        "sessions": [open_do_row("claude", "sess-prim")],
    })
}

/// A home staged with one graph fixture set; the caller adds registry rows.
fn primary_home(
    entries: Vec<Value>,
) -> (
    tempfile::TempDir,
    AgentsHome,
    EventEmitter,
    tempfile::TempDir,
) {
    let (dir, home) = staged_graph_home();
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let transcripts = tempfile::tempdir().unwrap();
    stage_graph(dir.path(), json!(entries));
    (dir, home, emitter, transcripts)
}

fn primary_registry_row(home: &AgentsHome) {
    state::update_registry(&home.registry_json(), |r| {
        spawn_row(r, "row-prim", "sess-prim");
    })
    .unwrap();
}

#[test]
fn ac1_hp_a_merged_primary_answer_stamps_and_settles() {
    let (dir, home, emitter, transcripts) = primary_home(vec![held_primary_node("x-prim")]);
    primary_registry_row(&home);
    let quiet = quiet_transcript(transcripts.path(), "p.jsonl", 2 * 3600);
    let mut read = |path: &str, _cwd: &str| {
        assert_eq!(path, "repos/o/r/pulls/2180");
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
    let rows = crate::graph_store::read_rows(&dir.path().join("graph.json")).unwrap();
    assert_eq!(rows[0]["merge_status"], json!("merged"));
    assert_eq!(
        summary.settled_do_rows,
        vec![("x-prim".into(), "claude".into(), "sess-prim".into())]
    );
    assert!(summary.retired.iter().any(|(id, _)| id == "row-prim"));
    assert_eq!(rows[0]["sessions"][0]["ended_by"], json!("reap-sweep"));
}

#[test]
fn ac1_err_an_open_or_unreadable_primary_stamps_nothing() {
    for answer in [Some(gc_sweep::PrState::Open), None] {
        let (dir, home, _emitter, _t) = primary_home(vec![held_primary_node("x-prim")]);
        let mut read = |_path: &str, _cwd: &str| answer;
        let (settled, refused) = gc_sweep::settle_stale_do_rows_with(&home, &mut read);
        assert!(settled.is_empty());
        assert!(refused.is_empty(), "{:?}", refused);
        let rows = crate::graph_store::read_rows(&dir.path().join("graph.json")).unwrap();
        assert_eq!(rows[0]["merge_status"], json!(null));
        assert!(rows[0]["sessions"][0]
            .as_object()
            .unwrap()
            .get("ended_at")
            .is_none());
    }
}

#[test]
fn ac1_edge_a_recorded_failure_or_a_rowless_node_pays_no_read() {
    let held_failed = json!({
        "id": "x-fail", "title": "failed", "slug": "x-fail", "type": "feature",
        "status": "done", "priority": "p2",
        "created_at": "2026-09-11T00:00:00+00:00",
        "completed_at": "2026-09-11T02:00:00+00:00",
        "merge_status": "failed",
        "pr_number": 2100,
        "pr_url": "https://github.com/o/r/pull/2100",
        "cwd": "/repo/wt",
        "sessions": [open_do_row("claude", "sess-fail")],
    });
    let rowless = json!({
        "id": "x-rowless", "title": "rowless", "slug": "x-rowless", "type": "feature",
        "status": "done", "priority": "p2",
        "created_at": "2026-09-11T00:00:00+00:00",
        "completed_at": "2026-09-11T02:00:00+00:00",
        "merge_status": json!(null),
        "pr_number": 2101,
        "pr_url": "https://github.com/o/r/pull/2101",
        "cwd": "/repo/wt",
    });
    let (dir, home, emitter, transcripts) = primary_home(vec![held_failed, rowless]);
    state::update_registry(&home.registry_json(), |r| {
        spawn_row(r, "row-fail", "sess-fail");
    })
    .unwrap();
    let quiet = quiet_transcript(transcripts.path(), "e.jsonl", 2 * 3600);
    let mut read = |_path: &str, _cwd: &str| panic!("no read may be paid");
    let summary = settle_staged_then_run(
        &home,
        &emitter,
        &move |_| Some(vec![quiet.clone()]),
        &mut read,
    );

    let rows = crate::graph_store::read_rows(&dir.path().join("graph.json")).unwrap();
    assert_eq!(rows[0]["merge_status"], json!("failed"));
    assert_eq!(rows[1]["merge_status"], json!(null));
    assert!(summary.settled_do_rows.is_empty());
    let hold = summary
        .holds
        .iter()
        .find(|hold| hold.id == "row-fail")
        .expect("the failed-merge row carries a hold");
    assert_eq!(hold.detail, "merge_status: failed");
}

fn dry_run_with(
    home: &AgentsHome,
    emitter: &EventEmitter,
    planned: &[gc_sweep::StaleDoRow],
    stamps: &[gc_sweep::PrStamp],
    transcripts: &dyn Fn(&state::RegistryEntry) -> Option<Vec<std::path::PathBuf>>,
) -> GcSummary {
    let read_graph = |h: &AgentsHome| {
        gc_sweep::read_graph_entries(h).map(|g| gc_sweep::without_settled(g, planned, stamps))
    };
    let mut summary = gc_sweep::run(
        home,
        emitter,
        0,
        true,
        0,
        &read_graph,
        transcripts,
        &staged_ages(transcripts),
        &|_| true,
        &|_| crate::daemon::CascadeOutcome::NotApplicable,
        &|_e| crate::daemon::CascadeOutcome::NotApplicable,
        &no_agents,
        &|_| (None, None),
        &|_| None,
    );
    summary.settled_do_rows = planned
        .iter()
        .map(|row| {
            (
                row.node.clone(),
                row.harness.clone(),
                row.session_id.clone(),
            )
        })
        .collect();
    summary
}

#[test]
fn ac3_hp_the_dry_run_rehearses_the_primary_stamp() {
    let (dir, home, emitter, transcripts) = primary_home(vec![held_primary_node("x-prim")]);
    primary_registry_row(&home);
    let quiet = quiet_transcript(transcripts.path(), "p2.jsonl", 2 * 3600);
    let version_before = crate::backlog::version(&dir.path().join("graph.json")).unwrap();
    let mut read = |path: &str, _cwd: &str| {
        assert_eq!(path, "repos/o/r/pulls/2180");
        Some(gc_sweep::PrState::Merged)
    };
    let (planned, stamps) = crate::additional_prs::plan_settle(&home, &mut read);
    assert_eq!(planned.len(), 1);
    assert_eq!(stamps.len(), 1);
    assert!(stamps[0].primary);
    assert_eq!(stamps[0].number, 2180);
    let summary = dry_run_with(&home, &emitter, &planned, &stamps, &move |_| {
        Some(vec![quiet.clone()])
    });
    assert_eq!(
        summary.settled_do_rows,
        vec![("x-prim".into(), "claude".into(), "sess-prim".into())]
    );
    assert!(
        summary.kept_open_do_row.is_empty(),
        "{:?}",
        summary.kept_open_do_row
    );
    assert!(!summary
        .holds
        .iter()
        .any(|hold| hold.id == "row-prim" && hold.reason == "open do row on done node"));
    let version_after = crate::backlog::version(&dir.path().join("graph.json")).unwrap();
    assert_eq!(version_before, version_after);
}

#[test]
fn ac3_edge_a_primary_stamp_and_an_open_extra_rehearse_the_real_hold() {
    let mut node = held_primary_node("x-prim");
    node["additional_prs"] = json!([{"number": 1523, "url": "https://github.com/o/r/pull/1523"}]);
    let (_dir, home, emitter, transcripts) = primary_home(vec![node]);
    primary_registry_row(&home);
    let quiet = quiet_transcript(transcripts.path(), "p3.jsonl", 2 * 3600);
    let mut answers: std::collections::HashMap<String, Option<gc_sweep::PrState>> =
        std::collections::HashMap::new();
    answers.insert(
        "repos/o/r/pulls/2180".to_string(),
        Some(gc_sweep::PrState::Merged),
    );
    answers.insert(
        "repos/o/r/pulls/1523".to_string(),
        Some(gc_sweep::PrState::Open),
    );
    let mut read = |path: &str, _cwd: &str| answers.get(path).copied().flatten();
    let (planned, stamps) = crate::additional_prs::plan_settle(&home, &mut read);
    assert!(
        planned.is_empty(),
        "the open extra keeps the row out of the settle plan"
    );
    assert_eq!(stamps.len(), 1);
    assert!(stamps[0].primary);
    let staged = gc_sweep::read_graph_entries(&home).unwrap();
    let subtracted = gc_sweep::without_settled(staged, &planned, &stamps);
    assert_eq!(
        subtracted.pr_state["x-prim"],
        (Some("merged".to_string()), 1, 1)
    );
    let summary = dry_run_with(&home, &emitter, &planned, &stamps, &move |_| {
        Some(vec![quiet.clone()])
    });
    let hold = summary
        .holds
        .iter()
        .find(|hold| hold.id == "row-prim")
        .expect("the held row carries a hold");
    assert_eq!(hold.detail, "additional_prs: 1 of 1 not recorded merged");
    assert_eq!(
        summary.kept_open_do_row,
        vec![("row-prim".to_string(), "x-prim".to_string())]
    );
}
