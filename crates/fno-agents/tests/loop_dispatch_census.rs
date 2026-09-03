//! The telemetry trap the dispatch census exists to close.
//!
//! A green board read is not evidence the loop works: the 70-second board run
//! also returned a valid 48KB payload and was still killed. And iteration 1
//! has already happened once under a broken board. The positive marker is a
//! `loop_unit_dispatched` row naming a real crown scope at iteration 2 or
//! higher, and a health read must separate fixture titles from real ones -
//! a journal of only fixture rows reports ZERO real dispatches, never 756.

use fno_agents::loop_runtime::{census_loop_unit_dispatches, is_fixture_dispatch_title};

fn row(title: &str, iteration: u64) -> String {
    format!(
        r#"{{"ts":"2026-09-02T22:00:00Z","type":"loop_unit_dispatched","source":"loop","data":{{"unit_id":"u1","session_id":"s1","iteration":{iteration},"title":"{title}"}}}}"#
    )
}

#[test]
fn a_journal_of_only_fixture_rows_reports_zero_real_dispatches() {
    // AC5-ERR: every fixture title the suites mint, and nothing else. 756
    // fixture rows must never read as a busy subsystem.
    let journal: String = [
        "king reign over epic-x",
        "persist history test",
        "ceiling mission",
        "real driver test",
        "happy path mission",
        "env test",
    ]
    .iter()
    .enumerate()
    .map(|(i, t)| format!("{}\n", row(t, (i % 5 + 1) as u64)))
    .collect();
    let census = census_loop_unit_dispatches(&journal);
    assert_eq!(census.fixture, 6);
    assert_eq!(census.real, 0);
    assert_eq!(census.real_at_iteration_two_or_later, 0);
}

#[test]
fn the_positive_marker_is_a_real_crown_scope_at_iteration_two_or_later() {
    // AC5-HP's shape: `king reign over fno` at iteration 2 clears the bar;
    // the same title at iteration 1 does not (AC5-EDGE - iteration 1 already
    // happened once under a board that could not be read twice).
    let journal = format!(
        "{}\n{}\n",
        row("king reign over fno", 1),
        row("king reign over fno", 2)
    );
    let census = census_loop_unit_dispatches(&journal);
    assert_eq!(census.real, 2);
    assert_eq!(census.real_at_iteration_two_or_later, 1);
}

#[test]
fn malformed_lines_and_other_events_are_skipped_not_counted() {
    let journal = format!(
        "not json at all\n{}\n{}\n",
        row("king reign over fno", 3),
        r#"{"ts":"2026-09-02T22:00:01Z","type":"loop_terminated","source":"loop","data":{"reason":"DonePRGreen"}}"#
    );
    let census = census_loop_unit_dispatches(&journal);
    assert_eq!(census.real, 1);
    assert_eq!(census.fixture, 0);
}

#[test]
fn fixture_titles_answer_true_and_real_scopes_answer_false() {
    assert!(is_fixture_dispatch_title("king reign over epic-x"));
    assert!(is_fixture_dispatch_title(""));
    assert!(is_fixture_dispatch_title("  "));
    assert!(!is_fixture_dispatch_title("king reign over fno"));
    assert!(!is_fixture_dispatch_title("king reign over epic-9f2a"));
}
