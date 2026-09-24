//! The backlog board's view tests, mounted by `backlog_board.rs`. Fixtures
//! build `Inputs` the way `backlog_model_tests.rs` does: pure functions
//! over fixture rows, no store, no process.

use super::*;
use crate::backlog_model;
use serde_json::json;

fn board_inputs() -> backlog_model::Inputs {
    let mut inp = backlog_model::Inputs::default();
    inp.backend = "graph".into();
    inp.order = vec!["x-1".into(), "x-2".into(), "x-3".into()];
    inp.rows = vec![
        json!({"id": "x-1", "status": "ready", "priority": "p1", "title": "First card", "project": "fno"}),
        json!({"id": "x-2", "status": "in_progress", "priority": "p2", "title": "mux card", "project": "fno"}),
        json!({"id": "x-3", "status": "ready", "priority": "p2", "title": "Other project", "project": "other"}),
    ];
    inp.flow = json!({"available": false, "reason": "fixture"});
    inp
}

fn board_with(inputs: backlog_model::Inputs) -> BoardView {
    let mut b = BoardView::new(0);
    b.inputs = Some(inputs);
    let q = b.query.to_query().expect("the default query parses");
    b.body = Some(backlog_model::board(b.inputs.as_ref().unwrap(), &q));
    b
}

// AC6-HP: before any gather the body says `reading board...`, never an
// empty board.
#[test]
fn reading_board_line_shows_before_the_first_gather() {
    let b = BoardView::new(0);
    let (lines, follow) = render(&b, 120);
    assert_eq!(lines[0], "reading board...");
    assert!(follow.is_none());
}

// AC4-HP: the stats line counts every column and renders the flow line.
#[test]
fn stats_line_counts_columns_and_names_flow() {
    let b = board_with(board_inputs());
    let (lines, _) = render(&b, 200);
    let stats = &lines[0];
    for word in [
        "In Progress",
        "Now",
        "Next",
        "Later",
        "Triage",
        "Done",
        "│",
        "flow: fixture",
    ] {
        assert!(stats.contains(word), "stats line missing {word}: {stats}");
    }
}

// AC5-HP: `L` cycles project -> epic -> none and keeps the cursor on the
// same card while the new grouping still shows it.
#[test]
fn lanes_cycle_keeps_the_cursor_card() {
    let mut b = board_with(board_inputs());
    first_card(&mut b);
    let card = cursor_card_id(&b);
    cycle_lanes_b(&mut b);
    assert!(matches!(b.query.lanes, backlog_model::LanesBy::Epic));
    assert_eq!(cursor_card_id(&b), card, "cursor keeps its card");
    cycle_lanes_b(&mut b);
    assert!(matches!(b.query.lanes, backlog_model::LanesBy::None));
    cycle_lanes_b(&mut b);
    assert!(
        matches!(b.query.lanes, backlog_model::LanesBy::Project),
        "third press returns to project"
    );
}

// AC6-ERR: a failed read never repaints a good board empty.
#[test]
fn failed_gather_keeps_the_last_good_board() {
    let mut b = board_with(board_inputs());
    let before = b.body.as_ref().unwrap().lanes.len();
    let mut bad = board_inputs();
    bad.rows_error = Some("the store read failed".into());
    b.inputs = Some(bad);
    rederive(&mut b);
    assert_eq!(
        b.body.as_ref().unwrap().lanes.len(),
        before,
        "last good board kept"
    );
    assert!(b.errors.iter().any(|e| e.contains("the store read failed")));
    let (lines, _) = render(&b, 200);
    assert!(lines.iter().any(|l| l.starts_with("! ")), "{lines:?}");
}

// AC7-EDGE: an unavailable flow renders its reason, never numbers.
#[test]
fn flow_unavailable_names_the_reason() {
    let mut inp = board_inputs();
    inp.flow = json!({"available": false, "reason": "no usable window start"});
    let b = board_with(inp);
    let (lines, _) = render(&b, 200);
    assert!(
        lines[0].contains("flow: no usable window start"),
        "{:?}",
        lines[0]
    );
}

// AC8-EDGE: below WIDE_CELLS_AT the cells stack with `Now  12` headers.
#[test]
fn narrow_layout_stacks_the_cells() {
    let b = board_with(board_inputs());
    let (lines, _) = render(&b, 80);
    let stacked = lines
        .iter()
        .filter(|l| {
            l.starts_with("In Progress  ") || l.starts_with("Now  ") || l.starts_with("Next  ")
        })
        .count();
    assert!(stacked >= 3, "expected stacked headers, got {lines:?}");
}

// AC11-EDGE: a filter matching nothing totals zero and says so.
#[test]
fn empty_filter_match_totals_zero_and_names_itself() {
    let mut b = board_with(board_inputs());
    b.query.q = Some("zzz-no-such-card".into());
    rederive(&mut b);
    let total: usize = b
        .body
        .as_ref()
        .unwrap()
        .lanes
        .iter()
        .map(|l| l.cells.iter().map(|c| c.total).sum::<usize>())
        .sum();
    assert_eq!(total, 0, "no cards match");
    let (lines, _) = render(&b, 120);
    assert!(
        lines.iter().any(|l| l.contains("no cards match")),
        "{lines:?}"
    );
}
