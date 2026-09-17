//! The question-sweep test family: an open question whose node reads done or
//! superseded closes with reason node-closed; an in-progress node's question
//! stays open. These pin the stamp, the fold, and the closed-row write so a
//! question never again outlives its work silently.

use super::*;

/// Fixture reading: statuses plus journal raw text, injected so the test
/// drives the sweep against a temp home instead of the live graph.
fn fixture_read(
    statuses: Vec<(String, String)>,
    raw: String,
) -> impl Fn(&Path, &AgentsHome) -> (Vec<(String, String)>, String) {
    move |_, _| (statuses.clone(), raw.clone())
}

fn open_question_row(qid: &str, node: &str) -> String {
    format!(
        r#"{{"ts":"2026-09-17T10:00:00Z","type":"operator_question","source":"target","data":{{"question_id":"{qid}","question":"which way?","node":"{node}","blocks":["{node}"]}}}}"#
    )
}

#[test]
fn question_sweep_closes_a_done_node_s_open_question() {
    let home = tmp_home("question-sweep-closes");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let read = fixture_read(
        vec![("x-done".to_string(), "done".to_string())],
        open_question_row("q-done", "x-done"),
    );
    let now = 1_000_000;

    assert_eq!(question_sweep_in(&home, &emitter, now, &read), 1);
    let questions =
        std::fs::read_to_string(crate::provider_cap::questions_path(&home)).unwrap_or_default();
    assert!(questions.contains("q-done"), "questions: {questions}");
    assert!(questions.contains("node-closed"), "questions: {questions}");
    let log = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
    assert!(log.contains("question_sweep"));
    assert!(log.contains("\"closed\":1"));
    // The cadence stamp holds: an immediate re-run sweeps nothing.
    assert_eq!(question_sweep_in(&home, &emitter, now + 60, &read), 0);
}

#[test]
fn question_sweep_leaves_an_in_progress_node_s_question_open() {
    let home = tmp_home("question-sweep-inprogress");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let read = fixture_read(
        vec![("x-live".to_string(), "in_progress".to_string())],
        open_question_row("q-live", "x-live"),
    );

    assert_eq!(
        question_sweep_in(
            &home,
            &emitter,
            1_000_000,
            &fixture_read(
                vec![("x-live".to_string(), "in_progress".to_string())],
                open_question_row("q-live", "x-live"),
            )
        ),
        0
    );
    let questions =
        std::fs::read_to_string(crate::provider_cap::questions_path(&home)).unwrap_or_default();
    assert!(!questions.contains("q-live"));
    let log = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
    assert!(log.contains("\"closed\":0"));
}

#[test]
fn question_sweep_honours_a_blocks_only_reference() {
    // The `node` field may be absent; a blocks entry naming a done node is
    // the same held work and closes the same way.
    let home = tmp_home("question-sweep-blocks");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let raw = r#"{"ts":"2026-09-17T10:00:00Z","type":"operator_question","source":"target","data":{"question_id":"q-blk","question":"pick","blocks":["x-done"]}}"#.to_string();
    let read = fixture_read(vec![("x-done".to_string(), "done".to_string())], raw);
    assert_eq!(question_sweep_in(&home, &emitter, 1_000_000, &read), 1);
    let questions =
        std::fs::read_to_string(crate::provider_cap::questions_path(&home)).unwrap_or_default();
    assert!(questions.contains("q-blk"));
}
