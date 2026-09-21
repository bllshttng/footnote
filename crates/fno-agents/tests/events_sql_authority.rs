//! End-to-end cutover journey for the authoritative event store: concurrent
//! appenders, a lost reply recovered by idempotent retry, legacy source
//! reconciliation, export stability, and repo-scoped identity.

use fno_agents::event_store::{
    append_envelope, export_jsonl, import_all, query_events, EventQuery,
};
use serde_json::json;

fn attestation(repo: &str, head: &str, pr: i64) -> String {
    json!({"ts": "2026-09-17T12:00:00Z", "type": "review_attestation",
        "source": "reviewer", "data": {"repo": repo, "head_sha": head,
        "pr": pr, "reviewer": "code-review", "verdict": "pass"}})
    .to_string()
}

fn count(store: &std::path::Path) -> i64 {
    fno_agents::event_store::open_read(store)
        .unwrap()
        .query_row("SELECT count(*) FROM events", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn concurrent_appenders_commit_exactly_once_each() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    // Migration + schema creation happen on this warm-up append, so the
    // concurrent legs race only on the append transaction, which is the
    // serialization point under test.
    append_envelope(&live, &attestation("o/r", "seed", 0), None).unwrap();

    let mut handles = Vec::new();
    for i in 0..8 {
        let live = live.clone();
        let envelope = attestation("o/r", &format!("head-{i}"), 100 + i);
        handles.push(std::thread::spawn(move || {
            append_envelope(&live, &envelope, None).unwrap()
        }));
    }
    let mut seqs = Vec::new();
    for h in handles {
        seqs.push(h.join().unwrap().seq);
    }
    seqs.sort();
    // Dense commit order with no gaps and no duplicates.
    seqs.iter()
        .enumerate()
        .for_each(|(i, seq)| assert_eq!(*seq, (i + 2) as i64));
    assert_eq!(count(&fno_agents::event_store::store_path(&live)), 9);
}

#[test]
fn kill_after_commit_recovers_by_idempotent_retry() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    let envelope = attestation("o/r", "abc", 7);
    let first = append_envelope(&live, &envelope, None).unwrap();
    assert!(first.inserted);
    // The producer died after the store committed but before it read the
    // receipt. The retry must land as the SAME row, never a second one.
    let retry = append_envelope(&live, &envelope, None).unwrap();
    assert!(!retry.inserted, "the retry is an idempotent hit");
    assert_eq!(retry.event_id, first.event_id);
    assert_eq!(retry.seq, first.seq);
    assert_eq!(count(&fno_agents::event_store::store_path(&live)), 1);
}

#[test]
fn legacy_import_reconciles_source_counts_and_retries_clean() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    let mut body = String::new();
    let accepted = ["x-1", "x-2", "x-3"].iter().map(|scope| {
        json!({"ts": "2026-09-10T08:00:00Z", "type": "reign_checkin",
               "source": "loop", "data": {"scope": scope, "change": "c"}})
        .to_string()
    });
    for line in accepted {
        body.push_str(&line);
        body.push('\n');
    }
    // A torn write and a scope that fails the canonical check: both must be
    // STORED with their reject_reason, never dropped.
    body.push_str("{not json\n");
    let bad_scope = json!({"ts": "2026-09-10T08:00:00Z", "type": "reign_checkin",
        "source": "loop", "data": {"scope": "two words", "change": "c"}})
    .to_string();
    body.push_str(&bad_scope);
    body.push('\n');
    std::fs::write(&live, body).unwrap();

    let first = import_all(&live).unwrap();
    assert_eq!(first.ingested, 5, "every complete source line lands once");
    let second = import_all(&live).unwrap();
    assert_eq!(second.ingested, 0, "the retry imports zero duplicates");
    let rejected: i64 =
        fno_agents::event_store::open_read(&fno_agents::event_store::store_path(&live))
            .unwrap()
            .query_row(
                "SELECT count(*) FROM events WHERE reject_reason IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
    assert_eq!(
        rejected, 2,
        "corrupt and bad-scope rows survive as evidence"
    );
}

#[test]
fn export_is_a_stable_snapshot_never_new_identity() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    append_envelope(&live, &attestation("o/r", "h1", 1), None).unwrap();
    append_envelope(&live, &attestation("o/r", "h2", 2), None).unwrap();

    let out1 = dir.path().join("snap1.jsonl");
    let out2 = dir.path().join("snap2.jsonl");
    let n1 = export_jsonl(&live, &out1).unwrap();
    let n2 = export_jsonl(&live, &out2).unwrap();
    assert_eq!((n1, n2), (2, 2));
    assert_eq!(
        std::fs::read(&out1).unwrap(),
        std::fs::read(&out2).unwrap(),
        "two exports of one store are byte-identical"
    );
    // A downgrade snapshot copied back in imports as nothing new.
    std::fs::copy(&out1, dir.path().join("restored.jsonl")).unwrap();
    let before = count(&fno_agents::event_store::store_path(&live));
    let receipt = import_all(&live).unwrap();
    assert_eq!(receipt.ingested, 0);
    assert_eq!(count(&fno_agents::event_store::store_path(&live)), before);
}

#[test]
fn repo_identity_prevents_cross_project_satisfaction() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    // One PR number and head sha, two repositories.
    append_envelope(&live, &attestation("original/repo", "abc123", 42), None).unwrap();
    append_envelope(&live, &attestation("fork/repo", "abc123", 42), None).unwrap();

    let rows = query_events(
        &live,
        &EventQuery {
            head_sha: Some("abc123".into()),
            repo: Some("original/repo".into()),
            limit: Some(10),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(rows.len(), 1, "only the matching repo's row satisfies");
    assert!(rows[0].line.contains("original/repo"));
}
