//! Store tests: ingest generations, v1-to-v2 migration, retention classes,
//! identity extraction, rejected-row preservation.

use super::*;
use serde_json::json;
use std::io::Write;

fn append(path: &Path, rows: &[serde_json::Value]) {
    let mut fh = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    for row in rows {
        writeln!(fh, "{row}").unwrap();
    }
}

fn checkin(ts: &str, scope: &str, change: &str) -> serde_json::Value {
    json!({"ts": ts, "type": "reign_checkin", "source": "loop",
           "data": {"scope": scope, "change": change}})
}

fn count_events(store: &Path) -> i64 {
    open_read(store)
        .unwrap()
        .query_row("SELECT count(*) FROM events", [], |r| r.get(0))
        .unwrap()
}

fn count_type(store: &Path, event_type: &str) -> i64 {
    open_read(store)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM events WHERE type = ?1",
            params![event_type],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn store_path_strips_generation_suffix() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    assert_eq!(store_path(&live), dir.path().join("events.db"));
    let rotated = dir.path().join("events.jsonl.1");
    assert_eq!(store_path(&rotated), dir.path().join("events.db"));
    let named = dir.path().join("global.jsonl");
    assert_eq!(store_path(&named), dir.path().join("global.db"));
}

#[test]
fn sync_ingests_both_generations_and_second_sync_is_zero() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    append(
        &live,
        &[checkin("2026-09-10T12:00:00Z", "x-aaaa", "newest")],
    );
    let rotated = dir.path().join("events.jsonl.1");
    append(
        &rotated,
        &[
            checkin("2026-09-09T08:00:00Z", "x-aaaa", "older"),
            checkin("2026-09-09T09:00:00Z", "x-other", "elsewhere"),
        ],
    );
    let first = sync(&live).unwrap();
    assert_eq!(first.ingested, 3);
    assert_eq!(count_events(&first.store), 3);
    let second = sync(&live).unwrap();
    assert_eq!(second.ingested, 0, "the cursor resumes, nothing re-ingests");
    assert_eq!(count_events(&second.store), 3);
}

#[test]
fn rotation_overwrite_keeps_ingested_history() {
    // Once `.1` is replaced by the next generation, the rows only the store
    // holds are still there.
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    let rotated = dir.path().join("events.jsonl.1");
    append(
        &rotated,
        &[checkin("2026-09-09T08:00:00Z", "x-aaaa", "gen 1")],
    );
    append(&live, &[checkin("2026-09-10T08:00:00Z", "x-aaaa", "gen 2")]);
    sync(&live).unwrap();
    // The rename: gen 2 becomes the new .1, gen 3 lands live.
    std::fs::rename(&live, &rotated).unwrap();
    append(&live, &[checkin("2026-09-11T08:00:00Z", "x-aaaa", "gen 3")]);
    let receipt = sync(&live).unwrap();
    assert_eq!(receipt.ingested, 1, "only gen 3 is new");
    assert_eq!(count_events(&receipt.store), 3);
    assert_eq!(count_type(&receipt.store, "reign_checkin"), 3);
}

#[test]
fn gc_rewrite_dedupes_by_row_hash() {
    // A rewrite under a new inode with the same rows plus one.
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    append(
        &live,
        &[
            checkin("2026-09-10T08:00:00Z", "x-aaaa", "kept"),
            checkin("2026-09-10T09:00:00Z", "x-aaaa", "also kept"),
        ],
    );
    sync(&live).unwrap();
    std::fs::remove_file(&live).unwrap();
    append(
        &live,
        &[
            checkin("2026-09-10T08:00:00Z", "x-aaaa", "kept"),
            checkin("2026-09-10T09:00:00Z", "x-aaaa", "also kept"),
            checkin("2026-09-10T10:00:00Z", "x-aaaa", "post-gc"),
        ],
    );
    let receipt = sync(&live).unwrap();
    assert_eq!(receipt.ingested, 1, "only the new row inserts");
    assert_eq!(count_events(&receipt.store), 3);
}

#[test]
fn corrupt_and_bad_scope_rows_store_with_reject_reason() {
    // Nothing drops; the first failure names the cause.
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    append(
        &live,
        &[
            json!({"ts": "2026-09-14T22:28:43Z", "type": "reign_checkin", "source": "loop",
             "data": {"scope": "x-bbbb ready no build, idea", "change": "corrupted"}}),
        ],
    );
    // A genuinely non-JSON line, the way a torn write or foreign writer
    // leaves one.
    {
        let mut fh = std::fs::OpenOptions::new()
            .append(true)
            .open(&live)
            .unwrap();
        writeln!(fh, "{{not json").unwrap();
    }
    let receipt = sync(&live).unwrap();
    assert_eq!(receipt.corrupt, 1);
    assert_eq!(count_events(&receipt.store), 2, "both rows are stored");
    let conn = open_read(&receipt.store).unwrap();
    let bad_scope: Option<String> = conn
        .query_row(
            "SELECT reject_reason FROM events WHERE reject_reason IS NOT NULL
             AND reject_reason != 'corrupt json'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        bad_scope.as_deref(),
        Some("scope is not a canonical crown scope")
    );
    let corrupt: i64 = conn
        .query_row(
            "SELECT count(*) FROM events WHERE reject_reason = 'corrupt json'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(corrupt, 1);
}

#[test]
fn canonical_scope_stamps_the_column() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    append(&live, &[checkin("2026-09-10T08:00:00Z", "x-aaaa", "clean")]);
    let receipt = sync(&live).unwrap();
    assert_eq!(count_type(&receipt.store, "reign_checkin"), 1);
}

#[test]
fn ephemeral_journal_is_refused_but_sibling_imports() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    append(
        &live,
        &[checkin("2026-09-10T08:00:00Z", "x-aaaa", "durable")],
    );
    let sibling = dir.path().join("events.jsonl.ephemeral");
    append(
        &sibling,
        &[
            json!({"ts": "2026-09-10T08:00:00Z", "type": "mux_pane_counters",
              "source": "mux", "data": {"panes": []}}),
        ],
    );
    let err = sync(&sibling).unwrap_err();
    assert!(err.contains("ephemeral"), "err: {err}");
    assert!(
        !store_path(&sibling).exists(),
        "no store was created for the sibling path"
    );
    // The cutover import DOES bring the sibling's rows in, classified.
    let receipt = import_all(&live).unwrap();
    assert_eq!(receipt.ingested, 2);
    assert_eq!(count_type(&receipt.store, "mux_pane_counters"), 1);
    let class: String = open_read(&receipt.store)
        .unwrap()
        .query_row(
            "SELECT retention_class FROM events WHERE type = 'mux_pane_counters'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(class, "ephemeral");
}

#[test]
fn prune_keeps_durable_and_gate_deletes_only_expired_ephemeral() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    append(
        &live,
        &[
            checkin("2026-05-01T08:00:00Z", "x-aaaa", "ancient durable"),
            checkin("2026-09-10T08:00:00Z", "x-aaaa", "fresh durable"),
            json!({"ts": "2026-05-01T08:00:00Z", "type": "review_attestation",
                   "source": "reviewer", "data": {"reviewer": "code-review",
                   "head_sha": "abc", "verdict": "pass"}}),
            json!({"ts": "2026-05-01T08:00:00Z", "type": "mux_pane_counters",
                   "source": "mux", "data": {"panes": []}}),
            json!({"ts": "2026-09-15T08:00:00Z", "type": "mux_pane_counters",
                   "source": "mux", "data": {"panes": []}}),
        ],
    );
    // A fresh store's first prune fires immediately (last_prune_ms starts at
    // 0), so the EXPIRED ephemeral row never survives even the first sync.
    let receipt = sync(&live).unwrap();
    let store = receipt.store;
    assert_eq!(count_events(&store), 4, "only the expired ephemeral left");
    assert_eq!(
        count_type(&store, "review_attestation"),
        1,
        "gate rows stay"
    );
    assert_eq!(count_type(&store, "reign_checkin"), 2, "durable rows stay");
    assert_eq!(
        count_type(&store, "mux_pane_counters"),
        1,
        "the fresh ephemeral row stays; the 2026-05-01 one is gone"
    );
    // Force the daily prune again; the fresh ephemeral row survives.
    let writable = Connection::open(&store).unwrap();
    writable
        .execute("DELETE FROM events_meta WHERE key = 'last_prune_ms'", [])
        .unwrap();
    drop(writable);
    sync(&live).unwrap();
    assert_eq!(
        count_events(&store),
        4,
        "a second prune deletes nothing new"
    );
    assert_eq!(count_type(&store, "mux_pane_counters"), 1);
}

#[test]
fn v1_store_migrates_in_place_oldest_first() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("events.db");
    let conn = Connection::open(&store).unwrap();
    conn.execute_batch(
        "CREATE TABLE events (
             row_hash BLOB PRIMARY KEY NOT NULL,
             ts_ms INTEGER NOT NULL,
             type TEXT NOT NULL,
             source TEXT NOT NULL,
             scope TEXT,
             reject_reason TEXT,
             line TEXT NOT NULL
         ) WITHOUT ROWID;
         CREATE INDEX events_scope_type_ts ON events(scope, type, ts_ms);
         CREATE TABLE ingest_cursor (
             dev INTEGER NOT NULL, ino INTEGER NOT NULL, head_hash BLOB NOT NULL,
             \"offset\" INTEGER NOT NULL, path TEXT NOT NULL, updated_ms INTEGER NOT NULL,
             PRIMARY KEY (dev, ino)
         );
         CREATE TABLE events_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )
    .unwrap();
    let lines = [
        checkin("2026-09-09T08:00:00Z", "x-aaaa", "older").to_string(),
        checkin("2026-09-10T12:00:00Z", "x-aaaa", "newest").to_string(),
        json!({"ts": "2026-05-01T08:00:00Z", "type": "review_attestation",
               "source": "reviewer", "data": {"reviewer": "code-review",
               "head_sha": "abc", "verdict": "pass"}})
        .to_string(),
    ];
    for line in &lines {
        let hash = Sha256::digest(line.as_bytes()).to_vec();
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        let ts = parse_rfc3339_ms(value["ts"].as_str().unwrap()).unwrap();
        let ty = if line.contains("review_attestation") {
            "review_attestation"
        } else {
            "reign_checkin"
        };
        conn.execute(
            "INSERT INTO events (row_hash, ts_ms, type, source, scope, reject_reason, line)
             VALUES (?1, ?2, ?3, 'test', NULL, NULL, ?4)",
            params![hash, ts, ty, line],
        )
        .unwrap();
    }
    drop(conn);

    // Opening through the store migrates in place.
    let live = dir.path().join("events.jsonl");
    let receipt = sync(&live).unwrap();
    assert_eq!(receipt.store, store);
    assert_eq!(count_events(&store), 3, "migration loses no row");
    let conn = open_read(&store).unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    let mut stmt = conn
        .prepare("SELECT seq, ts_ms, event_id, retention_class, line FROM events ORDER BY seq")
        .unwrap();
    let rows: Vec<(i64, i64, String, String, String)> = stmt
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rows.len(), 3);
    // Oldest-first seq, deterministic even though v1 had no order.
    let tss: Vec<i64> = rows.iter().map(|r| r.1).collect();
    let mut sorted = tss.clone();
    sorted.sort();
    assert_eq!(tss, sorted, "seq follows ts order");
    for (seq, row) in rows.iter().enumerate() {
        assert_eq!(row.0 as usize, seq + 1, "seq is 1-based and dense");
        let expected_hash = Sha256::digest(row.4.as_bytes()).to_vec();
        assert_eq!(row.2, format!("legacy:{}", hex(&expected_hash)));
    }
    assert!(
        rows.iter().any(|r| r.3 == "gate"),
        "the attestation row carries its gate class"
    );
    // A second open is a no-op, never a rebuild.
    drop(stmt);
    drop(conn);
    sync(&live).unwrap();
    assert_eq!(count_events(&store), 3);
}

#[test]
fn future_schema_is_refused_by_writers_and_readers_without_downgrade() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    let store = store_path(&live);
    let conn = Connection::open(&store).unwrap();
    conn.execute_batch(&format!(
        "CREATE TABLE events {EVENTS_V2_COLUMNS}; PRAGMA user_version = {};",
        SCHEMA_VERSION + 1
    ))
    .unwrap();
    drop(conn);

    let line = checkin("2026-09-10T12:00:00Z", "x-aaaa", "future").to_string();
    let write_error = append_envelope(&live, &line, None).unwrap_err();
    assert!(write_error.contains("newer than this build understands"));

    let read_error = query_events(&live, &EventQuery::default()).unwrap_err();
    assert!(read_error.contains("newer than this build understands"));

    let conn = Connection::open(&store).unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION + 1);
    let rows: i64 = conn
        .query_row("SELECT count(*) FROM events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 0);
}

#[test]
fn identity_columns_extract_from_envelope_data() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    append(
        &live,
        &[
            json!({"ts": "2026-09-10T08:00:00Z", "type": "review_attestation",
              "source": "reviewer", "data": {"session_id": "sess-1", "node": "x-9f2",
              "pr_number": 2170, "head_sha": "abc123", "repo": "bllshttng/footnote",
              "verdict": "pass"}}),
        ],
    );
    let receipt = sync(&live).unwrap();
    let conn = open_read(&receipt.store).unwrap();
    let (session, node, pr, head, repo): (
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<String>,
        Option<String>,
    ) = conn
        .query_row(
            "SELECT session_id, node_id, pr_number, head_sha, repo FROM events",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(session.as_deref(), Some("sess-1"));
    assert_eq!(node.as_deref(), Some("x-9f2"), "the `node` spelling lands");
    assert_eq!(pr, Some(2170));
    assert_eq!(head.as_deref(), Some("abc123"));
    assert_eq!(repo.as_deref(), Some("bllshttng/footnote"));
}

#[test]
fn retention_class_maps_schema_classes() {
    assert_eq!(retention_class("review_attestation"), "gate");
    assert_eq!(retention_class("review_coverage"), "gate");
    assert_eq!(retention_class("mux_pane_counters"), "ephemeral");
    assert_eq!(retention_class("reign_checkin"), "durable");
    assert_eq!(retention_class(""), "durable");
    assert!(is_gate_event("review_coverage"));
    assert!(is_ephemeral_event("single_flight_gate"));
}

#[test]
fn canonical_scope_normalizes_members() {
    assert_eq!(canonical_scope("b, a ,a"), "a,b");
    assert_eq!(canonical_scope(""), "");
}

#[test]
fn append_envelope_commits_and_reads_back() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    let envelope = json!({"ts": "2026-09-17T12:00:00Z", "type": "review_attestation",
        "source": "reviewer", "data": {"session_id": "s1", "node": "x-1",
        "pr": 2170, "head_sha": "abc", "repo": "o/r", "verdict": "pass"}})
    .to_string();
    let receipt = append_envelope(&live, &envelope, None).unwrap();
    assert!(receipt.inserted);
    assert_eq!(receipt.seq, 1);
    assert_eq!(receipt.retention_class, "gate");
    let rows = query_events(&live, &EventQuery::default()).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].line, envelope, "the envelope lands byte-for-byte");
    assert_eq!(rows[0].event_id, receipt.event_id);
    assert!(rows[0].event_id.starts_with("evt:"));
}

#[test]
fn append_retry_is_idempotent_hit_not_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    let envelope = checkin("2026-09-17T12:00:00Z", "x-aaaa", "once").to_string();
    let first = append_envelope(&live, &envelope, None).unwrap();
    assert!(first.inserted);
    let second = append_envelope(&live, &envelope, None).unwrap();
    assert!(
        !second.inserted,
        "a byte-identical retry is an idempotent hit"
    );
    assert_eq!(second.event_id, first.event_id);
    assert_eq!(count_events(&store_path(&live)), 1);
}

#[test]
fn append_same_id_different_payload_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    let a = checkin("2026-09-17T12:00:00Z", "x-aaaa", "one payload").to_string();
    let b = checkin("2026-09-17T12:00:00Z", "x-aaaa", "DIFFERENT").to_string();
    append_envelope(&live, &a, Some("evt:requested")).unwrap();
    let err = append_envelope(&live, &b, Some("evt:requested")).unwrap_err();
    assert!(err.contains("identity collision"), "err: {err}");
    assert_eq!(count_events(&store_path(&live)), 1);
}

#[test]
fn append_refuses_newline_and_bad_scope_and_bad_ts() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    assert!(append_envelope(&live, "{\"a\":1}\n{\"b\":2}", None).is_err());
    let bad_scope = json!({"ts": "2026-09-17T12:00:00Z", "type": "reign_checkin",
        "source": "loop", "data": {"scope": "x-1 ready, two words"}})
    .to_string();
    let err = append_envelope(&live, &bad_scope, None).unwrap_err();
    assert!(err.contains("canonical crown scope"), "err: {err}");
    let bad_ts = json!({"ts": "not-a-time", "type": "reign_checkin",
        "source": "loop", "data": {}})
    .to_string();
    let err = append_envelope(&live, &bad_ts, None).unwrap_err();
    assert!(err.contains("RFC3339"), "err: {err}");
    assert!(
        !store_path(&live).exists(),
        "refused writes never create a store"
    );
}

#[test]
fn append_requires_type_source_data_object() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    for bad in [
        r#"{"ts": "2026-09-17T12:00:00Z", "source": "s", "data": {}}"#,
        r#"{"ts": "2026-09-17T12:00:00Z", "type": "", "source": "s", "data": {}}"#,
        r#"{"ts": "2026-09-17T12:00:00Z", "type": "t", "data": {}}"#,
        r#"{"ts": "2026-09-17T12:00:00Z", "type": "t", "source": "s", "data": [1]}"#,
        "[1,2]",
    ] {
        let err = append_envelope(&live, bad, None).unwrap_err();
        assert!(err.contains("envelope"), "{bad}: {err}");
    }
    assert!(
        !store_path(&live).exists(),
        "refused writes never create a store"
    );
}

#[test]
fn query_filters_narrow_and_identity_columns_match() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    let mk = |node: &str, verdict: &str| {
        json!({"ts": "2026-09-17T12:00:00Z", "type": "review_attestation",
            "source": "reviewer", "data": {"node": node, "head_sha": "h1",
            "verdict": verdict}})
        .to_string()
    };
    append_envelope(&live, &mk("x-1", "pass"), None).unwrap();
    append_envelope(&live, &mk("x-2", "fail"), None).unwrap();
    append_envelope(
        &live,
        &checkin("2026-09-17T13:00:00Z", "x-1", "c").to_string(),
        None,
    )
    .unwrap();

    let q = EventQuery {
        types: vec!["review_attestation".into()],
        node_id: Some("x-1".into()),
        ..Default::default()
    };
    let rows = query_events(&live, &q).unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].line.contains("pass"));
    let q = EventQuery {
        since_ms: parse_rfc3339_ms("2026-09-17T12:30:00Z"),
        ..Default::default()
    };
    assert_eq!(
        query_events(&live, &q).unwrap().len(),
        1,
        "only the later row"
    );
    let q = EventQuery {
        include_rejected: true,
        ..Default::default()
    };
    assert_eq!(query_events(&live, &q).unwrap().len(), 3);
}

#[test]
fn export_round_trips_commit_order() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    let a = checkin("2026-09-17T12:00:00Z", "x-aaaa", "first").to_string();
    let b = checkin("2026-09-17T12:01:00Z", "x-aaaa", "second").to_string();
    append_envelope(&live, &a, None).unwrap();
    append_envelope(&live, &b, None).unwrap();
    let out = dir.path().join("snapshot.jsonl");
    let n = export_jsonl(&live, &out).unwrap();
    assert_eq!(n, 2);
    let text = std::fs::read_to_string(&out).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines, vec![a.as_str(), b.as_str()]);
    assert_eq!(count_events(&store_path(&live)), 2);
}

#[test]
fn journal_text_reads_history_then_live_with_the_type_filter() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    let older = checkin("2026-09-17T12:00:00Z", "x-aaaa", "older");
    let other = json!({"ts": "2026-09-17T12:00:01Z", "type": "unwanted_kind",
        "source": "loop", "data": {}});
    let tail = checkin("2026-09-17T12:00:02Z", "x-aaaa", "tail");
    // A rotated generation holds the asked row and an unasked one; the live
    // file holds the tail alone.
    append(&dir.path().join("events.jsonl.1"), &[older.clone(), other]);
    append(&live, &[tail.clone()]);
    sync(&live).unwrap();
    let text = journal_text(&live, &["reign_checkin"]);
    assert_eq!(
        text,
        format!("{}\n{tail}\n", older.clone()),
        "rotated history first with the unasked type gone, live tail verbatim"
    );
    // The read never syncs: reading twice is stable.
    assert_eq!(journal_text(&live, &["reign_checkin"]), text);
}

#[test]
fn journal_text_with_no_type_filter_reads_every_committed_row() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    append_envelope(
        &live,
        &checkin("2026-09-17T12:00:00Z", "x-aaaa", "all").to_string(),
        None,
    )
    .unwrap();
    append_envelope(
        &live,
        &json!({"ts": "2026-09-17T12:01:00Z", "type": "loop_check",
            "source": "hook", "data": {"decision": "block"}})
        .to_string(),
        None,
    )
    .unwrap();

    let text = journal_text(&live, &[]);
    assert!(text.contains("reign_checkin"));
    assert!(text.contains("loop_check"));
}

#[test]
fn journal_text_preserves_append_order_for_equal_timestamps() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    let a = checkin("2026-09-17T12:00:00Z", "x-aaaa", "first");
    let b = checkin("2026-09-17T12:00:00Z", "x-aaaa", "second");
    append(&live, &[a, b]);
    sync(&live).unwrap();
    let text = journal_text(&live, &["reign_checkin"]);
    assert!(
        text.find("\"first\"").unwrap() < text.find("\"second\"").unwrap(),
        "same-second rows must retain append order: {text}"
    );
}

#[test]
fn journal_text_falls_back_to_the_live_file_when_the_store_breaks() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    append(&live, &[checkin("2026-09-10T12:00:00Z", "x-aaaa", "live")]);
    std::fs::create_dir(dir.path().join("events.db")).unwrap();
    let text = journal_text(&live, &["reign_checkin"]);
    assert_eq!(text, std::fs::read_to_string(&live).unwrap());
}

#[test]
fn journal_text_creates_no_store_for_an_absent_journal() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    assert_eq!(journal_text(&live, &["reign_checkin"]), "");
    assert!(!store_path(&live).exists());
}

#[test]
fn journal_text_reads_committed_rows_in_commit_order() {
    // AC1-HP: an imported row stays ahead of a store-only commit.
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    let a = checkin("2026-09-17T12:00:00Z", "x-aaaa", "imported");
    append(&live, &[a]);
    sync(&live).unwrap();
    let b = checkin("2026-09-17T12:01:00Z", "x-aaaa", "store-only");
    append_envelope(&live, &b.to_string(), None).unwrap();
    let text = journal_text(&live, &["reign_checkin"]);
    assert!(
        text.find("\"imported\"").unwrap() < text.find("\"store-only\"").unwrap(),
        "committed rows first, in commit order: {text}"
    );
    assert_eq!(text.matches("imported").count(), 1, "each row once: {text}");
    assert_eq!(
        text.matches("store-only").count(),
        1,
        "each row once: {text}"
    );
}

#[test]
fn journal_text_checked_errs_on_a_broken_store_and_journal_text_falls_back() {
    // AC1-ERR
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    append(&live, &[checkin("2026-09-10T12:00:00Z", "x-aaaa", "live")]);
    std::fs::write(dir.path().join("events.db"), b"not a database").unwrap();
    let err = journal_text_checked(&live, &EventQuery::of_types(&["reign_checkin"])).unwrap_err();
    assert!(err.contains("events.db"), "err names the store path: {err}");
    assert_eq!(
        journal_text(&live, &["reign_checkin"]),
        std::fs::read_to_string(&live).unwrap()
    );
}

#[test]
fn journal_text_appends_only_the_live_lines_the_store_lacks() {
    // AC1-EDGE: store rows A and B, then the un-imported live line C.
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    let a = checkin("2026-09-17T12:00:00Z", "x-aaaa", "row-a");
    let b = checkin("2026-09-17T12:01:00Z", "x-aaaa", "row-b");
    append_envelope(&live, &a.to_string(), None).unwrap();
    append_envelope(&live, &b.to_string(), None).unwrap();
    append(
        &live,
        &[checkin("2026-09-17T12:02:00Z", "x-aaaa", "raw-tail")],
    );
    let text = journal_text(&live, &[]);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "A, B, C: {text}");
    assert!(
        lines[0].contains("\"row-a\"")
            && lines[1].contains("\"row-b\"")
            && lines[2].contains("\"raw-tail\""),
        "commit order then the unseen tail: {text}"
    );
}

#[test]
fn journal_text_checked_window_does_not_readd_filtered_live_rows() {
    // AC1-WINDOW: a live row the since_ms window filtered stays out.
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    let a = checkin("2026-09-17T12:00:00Z", "x-aaaa", "old");
    let b = checkin("2026-09-17T12:01:00Z", "x-aaaa", "new");
    append(&live, &[a, b]);
    sync(&live).unwrap();
    let c = checkin("2026-09-17T12:02:00Z", "x-aaaa", "store-only");
    append_envelope(&live, &c.to_string(), None).unwrap();
    let q = EventQuery {
        since_ms: parse_rfc3339_ms("2026-09-17T12:00:30Z"),
        ..EventQuery::of_types(&[])
    };
    let text = journal_text_checked(&live, &q).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "B then C, no A: {text}");
    assert!(
        lines[0].contains("\"new\"") && lines[1].contains("\"store-only\""),
        "windowed commit order: {text}"
    );
}

mod drift;
