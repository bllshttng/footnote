use super::*;
use serde_json::json;
use std::sync::{Arc, Barrier, Mutex};

fn allow(ts: &str, tool: &str) -> serde_json::Value {
    json!({"ts": ts, "type": "guard_decision", "source": "hook",
           "data": {"guard": "git-protection", "decision": "allow", "tool": tool}})
}

#[test]
fn import_coalesces_identical_allow_polls() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    append(
        &live,
        &[
            allow("2026-09-10T12:00:00Z", "Bash"),
            allow("2026-09-10T12:01:00Z", "Bash"),
            allow("2026-09-10T12:02:00Z", "Bash"),
        ],
    );
    let receipt = sync(&live).unwrap();
    assert_eq!(receipt.ingested, 1, "only the transition row is stored");
    assert_eq!(receipt.coalesced, 2, "the identical polls are pending");
    assert_eq!(count_events(&store_path(&live)), 1);
    append(&live, &[allow("2026-09-10T12:10:00Z", "Bash")]);
    sync(&live).unwrap();
    let rows = query_events(&live, &EventQuery::default()).unwrap();
    assert_eq!(rows.len(), 3, "transition, summary, new transition");
    assert_eq!(rows[1].r#type, "guard_decision");
    assert!(
        rows[1].line.contains("\"occurrence_count\":2"),
        "{}",
        rows[1].line
    );
    assert!(rows[1].line.contains("\"window_started_ms\":"));
    assert!(rows[1].line.contains("\"window_finished_ms\":"));
}

#[test]
fn append_gate_suppresses_and_retry_is_idempotent_hit() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    let first = allow("2026-09-10T12:00:00Z", "Bash").to_string();
    let second = allow("2026-09-10T12:01:00Z", "Bash").to_string();
    let first_receipt = append_envelope(&live, &first, None).unwrap();
    assert!(first_receipt.inserted);
    assert!(!first_receipt.suppressed);
    let second_receipt = append_envelope(&live, &second, None).unwrap();
    assert!(!second_receipt.inserted);
    assert!(second_receipt.suppressed);
    assert_eq!(second_receipt.pending_occurrences, 1);
    let retry = append_envelope(&live, &first, None).unwrap();
    assert!(!retry.inserted);
    assert!(
        !retry.suppressed,
        "a byte-identical retry is an idempotent hit"
    );
    assert_eq!(retry.seq, first_receipt.seq);
    assert_eq!(count_events(&store_path(&live)), 1);
}

#[test]
fn blocks_and_malformed_never_coalesce() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    for ts in [
        "2026-09-10T12:00:00Z",
        "2026-09-10T12:01:00Z",
        "2026-09-10T12:02:00Z",
    ] {
        let block = json!({"ts": ts, "type": "guard_decision", "source": "hook",
            "data": {"guard": "git-protection", "decision": "block", "tool": "Bash"}});
        let receipt = append_envelope(&live, &block.to_string(), None).unwrap();
        assert!(receipt.inserted);
        assert!(
            !receipt.suppressed,
            "each block is a separate attempted action"
        );
    }
    let malformed = json!({"ts": "2026-09-10T12:03:00Z", "type": "guard_decision",
        "source": "hook", "data": {"guard": "git-protection", "tool": "Bash"}});
    let receipt = append_envelope(&live, &malformed.to_string(), None).unwrap();
    assert!(receipt.inserted, "a missing decision is always-audit");
    assert_eq!(count_events(&store_path(&live)), 4);
}

#[test]
fn undeclared_type_identical_payloads_stay_two_rows() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    let a = checkin("2026-09-10T12:00:00Z", "x-aaaa", "same").to_string();
    let b = checkin("2026-09-10T12:01:00Z", "x-aaaa", "same").to_string();
    let receipt_a = append_envelope(&live, &a, None).unwrap();
    let receipt_b = append_envelope(&live, &b, None).unwrap();
    assert!(receipt_a.inserted);
    assert!(receipt_b.inserted);
    assert_eq!(count_events(&store_path(&live)), 2);
}

#[test]
fn fingerprint_change_flushes_pending() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    append_envelope(
        &live,
        &allow("2026-09-10T12:00:00Z", "Bash").to_string(),
        None,
    )
    .unwrap();
    append_envelope(
        &live,
        &allow("2026-09-10T12:01:00Z", "Bash").to_string(),
        None,
    )
    .unwrap();
    append_envelope(
        &live,
        &allow("2026-09-10T12:02:00Z", "Bash").to_string(),
        None,
    )
    .unwrap();
    let changed = append_envelope(
        &live,
        &allow("2026-09-10T12:03:00Z", "Edit").to_string(),
        None,
    )
    .unwrap();
    assert!(changed.inserted);
    let rows = query_events(&live, &EventQuery::default()).unwrap();
    assert_eq!(
        rows.len(),
        3,
        "transition, flushed summary, changed transition"
    );
    assert!(
        rows[1].line.contains("\"occurrence_count\":2"),
        "{}",
        rows[1].line
    );
    assert!(
        rows[2].line.contains("\"tool\":\"Edit\""),
        "{}",
        rows[2].line
    );
}

#[test]
fn advance_subject_isolation() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    let mk = |ts: &str, node: &str| {
        json!({"ts": ts, "type": "advance_skipped", "source": "backlog",
               "data": {"reason": "no-work", "rank": "config", "closed_node_id": node}})
    };
    let a = append_envelope(
        &live,
        &mk("2026-09-10T12:00:00Z", "x-aaaa").to_string(),
        None,
    )
    .unwrap();
    let b = append_envelope(
        &live,
        &mk("2026-09-10T12:01:00Z", "x-bbbb").to_string(),
        None,
    )
    .unwrap();
    assert!(a.inserted && b.inserted);
    assert!(
        !a.suppressed && !b.suppressed,
        "different subjects never coalesce"
    );
    assert_eq!(count_events(&store_path(&live)), 2);
}

#[test]
fn concurrent_identical_first_polls_yield_one_transition() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    let receipts: Arc<Mutex<Vec<Result<AppendReceipt, String>>>> = Arc::new(Mutex::new(Vec::new()));
    let barrier = Arc::new(Barrier::new(8));
    let mut handles = Vec::new();
    for i in 0..8 {
        let live = live.clone();
        let barrier = barrier.clone();
        let receipts = receipts.clone();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            let ts = format!("2026-09-10T12:00:0{i}Z");
            let receipt = append_envelope(&live, &allow(&ts, "Bash").to_string(), None);
            receipts.lock().unwrap().push(receipt);
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }
    let receipts = receipts.lock().unwrap().drain(..).collect::<Vec<_>>();
    assert_eq!(receipts.len(), 8, "every thread returned a receipt");
    let inserted = receipts
        .iter()
        .filter(|r| matches!(r, Ok(rec) if rec.inserted))
        .count();
    let suppressed = receipts
        .iter()
        .filter(|r| matches!(r, Ok(rec) if rec.suppressed))
        .count();
    assert_eq!(inserted, 1, "exactly one transition row");
    assert_eq!(suppressed, 7, "the rest are pending occurrences");
    assert_eq!(count_events(&store_path(&live)), 1);
}

#[test]
fn replay_after_cursor_loss_does_not_double_count() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    append(
        &live,
        &[
            allow("2026-09-10T12:00:00Z", "Bash"),
            allow("2026-09-10T12:01:00Z", "Bash"),
            allow("2026-09-10T12:02:00Z", "Bash"),
            allow("2026-09-10T12:03:00Z", "Bash"),
        ],
    );
    let receipt = sync(&live).unwrap();
    assert_eq!(receipt.ingested, 1);
    assert_eq!(receipt.coalesced, 3);
    let store = store_path(&live);
    {
        let conn = Connection::open(&store).unwrap();
        conn.execute("DELETE FROM ingest_cursor", []).unwrap();
    }
    append(
        &live,
        &[checkin("2026-09-10T12:05:00Z", "x-aaaa", "cursor loss")],
    );
    sync(&live).unwrap();
    let conn = Connection::open(&store).unwrap();
    let guards: i64 = conn
        .query_row(
            "SELECT count(*) FROM events WHERE type = 'guard_decision'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(guards, 1, "the replayed transition does not insert twice");
    let pending: i64 = conn
        .query_row("SELECT count(*) FROM event_observation_pending", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(pending, 3, "the replayed polls do not double count");
    drop(conn);
    assert_eq!(count_events(&store), 2, "the new checkin imports");
}

#[test]
fn unavailable_observation_state_is_an_explicit_failure() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    append_envelope(
        &live,
        &allow("2026-09-10T12:00:00Z", "Bash").to_string(),
        None,
    )
    .unwrap();
    let store = store_path(&live);
    {
        let conn = Connection::open(&store).unwrap();
        conn.execute_batch(
            "DROP TABLE event_observation_state;
             CREATE TABLE event_observation_state (x INTEGER);",
        )
        .unwrap();
    }
    let broken = append_envelope(
        &live,
        &allow("2026-09-10T12:01:00Z", "Bash").to_string(),
        None,
    );
    let err = broken.unwrap_err();
    assert!(err.contains("event_observation_state"), "err: {err}");
    let conn = Connection::open(&store).unwrap();
    let pending: i64 = conn
        .query_row(
            "SELECT count(*) FROM event_observation_pending
             WHERE obs_type = 'guard_decision'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        pending, 0,
        "nothing counted pending when the state table is broken"
    );
    let guards: i64 = conn
        .query_row(
            "SELECT count(*) FROM events WHERE type = 'guard_decision'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(guards, 1, "no row was added beyond the first");
}

#[test]
fn flush_with_nothing_pending_closes_the_window_without_a_summary() {
    // A transition followed by a fingerprint change (or a heartbeat-expired
    // poll) with zero suppressed occurrences: the flush is a silent no-op,
    // never an error, and the new observation lands as an ordinary
    // transition.
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    let mk = |ts: &str, tool: &str| {
        json!({"ts": ts, "type": "guard_decision", "source": "hook",
               "data": {"guard": "git-protection", "decision": "allow", "tool": tool}})
    };
    let first =
        append_envelope(&live, &mk("2026-09-10T12:00:00Z", "Bash").to_string(), None).unwrap();
    assert!(first.inserted);
    let second =
        append_envelope(&live, &mk("2026-09-10T12:01:00Z", "Edit").to_string(), None).unwrap();
    assert!(second.inserted, "the changed poll inserts as a transition");
    assert!(!second.suppressed);
    // Heartbeat expiry with nothing pending is the same no-op.
    let third =
        append_envelope(&live, &mk("2026-09-10T12:20:00Z", "Edit").to_string(), None).unwrap();
    assert!(third.inserted);
    let rows = query_events(&live, &EventQuery::default()).unwrap();
    assert_eq!(rows.len(), 3, "three transitions, no summary row: {rows:?}");
    assert!(
        rows.iter().all(|r| !r.line.contains("occurrence_count")),
        "no summary row exists: {rows:?}"
    );
}
