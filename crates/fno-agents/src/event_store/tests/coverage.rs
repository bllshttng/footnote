use super::*;
use serde_json::json;

#[test]
fn imported_history_reads_partial_until_the_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    append(
        &live,
        &[
            checkin("2026-09-10T12:00:00Z", "x-aaaa", "before"),
            checkin("2026-09-10T12:01:00Z", "x-1cbb", "old"),
        ],
    );
    sync(&live).unwrap();
    // A since before the epoch is only partially proven even though rows
    // were imported from before it.
    let early = coverage(&live, Some(0), &["lead_checkin".to_string()]);
    assert_eq!(early.status, "partial", "{early:?}");
    let epoch = early.complete_since_ms.unwrap();
    assert!(
        early.observed_first_ms.unwrap() < epoch,
        "observed history precedes the epoch: {early:?}"
    );
    // A since at or after the epoch is proven complete.
    let late = coverage(&live, Some(epoch), &["lead_checkin".to_string()]);
    assert_eq!(late.status, "complete", "{late:?}");
    assert_eq!(late.complete_since_ms, Some(epoch));
}

#[test]
fn missing_unopenable_and_pre_epoch_stores_never_read_complete() {
    // No store at all: nothing is proven.
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    let missing = coverage(&live, Some(0), &[]);
    assert_eq!(missing.status, "unreadable", "{missing:?}");
    // A store file that is not a database backs no confident count.
    sync(&live).unwrap();
    let store = store_path(&live);
    std::fs::write(&store, b"not a database").unwrap();
    let garbage = coverage(&live, Some(0), &[]);
    assert_eq!(garbage.status, "unreadable", "{garbage:?}");
}

#[test]
fn pre_epoch_store_reads_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    append(&live, &[checkin("2026-09-10T12:00:00Z", "x-aaaa", "x")]);
    sync(&live).unwrap();
    let store = store_path(&live);
    rusqlite::Connection::open(&store)
        .unwrap()
        .execute(
            "DELETE FROM events_meta WHERE key = 'coverage_complete_since_ms'",
            [],
        )
        .unwrap();
    let unstamped = coverage(&live, Some(i64::MAX / 2), &[]);
    assert_eq!(unstamped.status, "unknown", "{unstamped:?}");
}

#[test]
fn ephemeral_proven_start_follows_the_prune_cutoff() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    append(
        &live,
        &[
            json!({"ts": "2026-09-10T12:00:00Z", "type": "human_touch", "source": "mux",
               "data": {"pane": "main"}}),
        ],
    );
    sync(&live).unwrap();
    let store = store_path(&live);
    let conn = rusqlite::Connection::open(&store).unwrap();
    // Rewrite the epoch into the deep past and stamp a prune whose cutoff
    // lands after it: the ephemeral kind then only proves back to the
    // cutoff, while a durable kind still proves back to the epoch.
    let epoch: i64 = 1_000;
    let prune_ms: i64 = (672 + 48) * 3_600_000;
    conn.execute(
        "UPDATE events_meta SET value = ?1 WHERE key = 'coverage_complete_since_ms'",
        params![epoch.to_string()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO events_meta (key, value) VALUES ('last_prune_ms', ?1)
         ON CONFLICT(key) DO UPDATE SET value = ?1",
        params![prune_ms.to_string()],
    )
    .unwrap();
    drop(conn);
    let ephemeral = coverage(
        &live,
        Some(prune_ms - 3_600_000),
        &["human_touch".to_string()],
    );
    assert_eq!(ephemeral.status, "complete", "{ephemeral:?}");
    assert_eq!(
        ephemeral.complete_since_ms,
        Some(prune_ms - 672 * 3_600_000)
    );
    let durable = coverage(&live, Some(epoch), &["lead_checkin".to_string()]);
    assert_eq!(durable.status, "complete", "{durable:?}");
    assert_eq!(durable.complete_since_ms, Some(epoch));
}
