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
    json!({"ts": ts, "type": "lead_checkin", "source": "loop",
           "data": {"scope": scope, "change": change}})
}

fn count_events(store: &Path) -> i64 {
    open_read(store)
        .unwrap()
        .query_row("SELECT count(*) FROM events", [], |r| r.get(0))
        .unwrap()
}

/// A timestamp inside the ephemeral TTL window on any run date. A fixed date
/// expires once the wall clock passes it by the TTL.
fn fresh_ts() -> String {
    (chrono::Utc::now() - chrono::Duration::hours(1))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
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

/// An ephemeral row that must survive import sits one hour old: import
/// prunes on a fresh store, so any fixed ts crosses the retention floor
/// and the row vanishes.
fn fresh_ts() -> String {
    (chrono::Utc::now() - chrono::Duration::hours(1))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[test]
fn a_cause_stores_reads_back_and_migrates_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("events.jsonl");
    let envelope = json!({
        "ts": "2026-10-04T00:00:00Z",
        "type": "agent_spawned",
        "source": "daemon",
        "data": {"caused_by": "evt:parent", "session_id": "s-1"},
    })
    .to_string();
    let receipt = append_envelope(&journal, &envelope, None).unwrap();
    assert!(receipt.inserted);
    let caused: Option<String> = open_read(&store_path(&journal))
        .unwrap()
        .query_row(
            "SELECT caused_by FROM events WHERE event_id = ?1",
            params![receipt.event_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(caused.as_deref(), Some("evt:parent"));
    // An envelope without a cause stores NULL, never an empty string.
    let plain = append_envelope(
        &journal,
        &checkin("2026-10-04T00:00:01Z", "x", "c").to_string(),
        None,
    )
    .unwrap();
    let caused: Option<String> = open_read(&store_path(&journal))
        .unwrap()
        .query_row(
            "SELECT caused_by FROM events WHERE event_id = ?1",
            params![plain.event_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(caused.is_none());
    // A store created before the column existed gains it in place.
    let store = store_path(&journal);
    {
        let conn = open_store(&store).unwrap();
        conn.execute_batch("ALTER TABLE events DROP COLUMN caused_by")
            .unwrap();
    }
    let mut conn = rusqlite::Connection::open(&store).unwrap();
    ensure_schema(&mut conn, &store).unwrap();
    let has: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('events') WHERE name = 'caused_by'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(has, 1);
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
    // A state-root journal routes through the layout ladder; a migrated root
    // answers the db/ store and a space journal keeps its sibling.
    let _guard = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let prior = std::env::var_os("FNO_AGENTS_HOME");
    std::env::set_var("FNO_AGENTS_HOME", dir.path().join("agents"));
    let root = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::create_dir_all(root.join("db")).unwrap();
    std::fs::write(root.join("db").join("events.db"), b"SQLite format 3\0").unwrap();
    let routed = store_path(&root.join("events.jsonl"));
    assert_eq!(
        routed,
        root.join("db").join("events.db"),
        "a migrated root answers the db/ store"
    );
    let alias = root.join("state-alias");
    std::os::unix::fs::symlink(&root, &alias).unwrap();
    assert_eq!(
        store_path(&alias.join("events.jsonl")),
        root.join("db").join("events.db"),
        "a virtual journal under a symlinked root keeps the migrated store"
    );
    let space = root.join("spaces").join("proj");
    std::fs::create_dir_all(&space).unwrap();
    assert_eq!(
        store_path(&space.join("events.jsonl")),
        space.join("events.db"),
        "a space journal keeps the sibling store"
    );
    match prior {
        Some(v) => std::env::set_var("FNO_AGENTS_HOME", v),
        None => std::env::remove_var("FNO_AGENTS_HOME"),
    }
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
    assert_eq!(count_type(&receipt.store, "lead_checkin"), 3);
    // The alias table rides the same read: a pre-rename row surfaces to a
    // new-spelling query.
    append(
        &live,
        &[json!({"ts": "2026-09-12T08:00:00Z", "type": "lead_checkin",
                 "source": "loop", "data": {"scope": "x-aaaa", "change": "old spelling"}})],
    );
    sync(&live).unwrap();
    let hits = query_events(
        &live,
        &EventQuery {
            types: vec!["lead_checkin".into()],
            ..Default::default()
        },
    )
    .unwrap();
    let changes: Vec<String> = hits
        .iter()
        .filter_map(|r| serde_json::from_str::<serde_json::Value>(&r.line).ok())
        .filter_map(|v| v["data"]["change"].as_str().map(|c| c.to_string()))
        .collect();
    assert_eq!(
        changes,
        ["gen 1", "gen 2", "gen 3", "old spelling"],
        "the alias table must surface the pre-rename row beside the new ones"
    );
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
            json!({"ts": "2026-09-14T22:28:43Z", "type": "lead_checkin", "source": "loop",
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
        Some("scope is not a canonical team scope")
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
        &[json!({"ts": fresh_ts(), "type": "mux_pane_counters",
              "source": "mux", "data": {"panes": []}})],
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
            json!({"ts": fresh_ts(), "type": "mux_pane_counters",
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
    assert_eq!(count_type(&store, "lead_checkin"), 2, "durable rows stay");
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
            "lead_checkin"
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
fn corrupt_pages_refuse_writes_at_statement_time_and_file_attention() {
    let dir = tempfile::tempdir().unwrap();
    let damaged = dir.path().join("damaged.jsonl");
    // Seed one stored row so the append must traverse the events b-tree.
    append(
        &damaged,
        &[checkin("2026-09-10T12:00:00Z", "x-aaaa", "seed")],
    );
    sync(&damaged).unwrap();
    let store = store_path(&damaged);
    let conn = Connection::open(&store).unwrap();
    let root: u64 = conn
        .query_row(
            "SELECT rootpage FROM sqlite_schema WHERE name='events'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let page_size: u64 = conn
        .query_row("PRAGMA page_size", [], |row| row.get(0))
        .unwrap();
    drop(conn);
    use std::io::{Seek, SeekFrom, Write};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(&store)
        .unwrap();
    file.seek(SeekFrom::Start((root - 1) * page_size)).unwrap();
    file.write_all(&vec![0; page_size as usize]).unwrap();
    drop(file);
    let line = checkin("2026-09-10T12:00:01Z", "x-aaaa", "damaged").to_string();
    let before = std::fs::read(&store).unwrap();
    let error = append_envelope(&damaged, &line, None).unwrap_err();
    assert!(
        error.contains("database disk image is malformed") && error.contains("write refused"),
        "{error}"
    );
    // The damaged page region itself must survive untouched (a lazy DDL
    // commit may still checkpoint elsewhere in the file), and the file
    // must never SHRINK.
    let after = std::fs::read(&store).unwrap();
    let page_start = ((root - 1) * page_size) as usize;
    let page_end = page_start + page_size as usize;
    assert_eq!(&after[page_start..page_end], &before[page_start..page_end]);
    assert!(after.len() >= before.len());
    let attention = std::fs::read_to_string(dir.path().join("questions.jsonl")).unwrap();
    assert!(attention.contains("event-store-integrity"));
    let items = crate::attention::project(&attention, &[], "", 0);
    assert_eq!(items.len(), 1);
    assert!(items[0].ready, "{:?}", items[0].missing);
}

#[test]
fn import_refuses_touching_damage_an_append_never_reads() {
    // No integrity sweep runs on any open (that scan per guard row drove the
    // fleet load storm): each door hits page damage only when a statement
    // touches it. Damage the ingest_cursor page: the import reads the cursor
    // and refuses, while the append never reads it and still lands.
    let dir = tempfile::tempdir().unwrap();
    let damaged = dir.path().join("damaged.jsonl");
    // Seed the journal and store so the import opens the cursor with real
    // work in front of it, then damage the cursor's page.
    append(
        &damaged,
        &[checkin("2026-09-10T11:00:00Z", "x-aaaa", "seed")],
    );
    sync(&damaged).unwrap();
    let store = store_path(&damaged);
    let conn = Connection::open(&store).unwrap();
    let root: u64 = conn
        .query_row(
            "SELECT rootpage FROM sqlite_schema WHERE name='ingest_cursor'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let page_size: u64 = conn
        .query_row("PRAGMA page_size", [], |row| row.get(0))
        .unwrap();
    drop(conn);
    use std::io::{Seek, SeekFrom, Write};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(&store)
        .unwrap();
    file.seek(SeekFrom::Start((root - 1) * page_size)).unwrap();
    file.write_all(&vec![0; page_size as usize]).unwrap();
    drop(file);
    let line = checkin("2026-09-10T12:00:00Z", "x-aaaa", "offpath").to_string();
    append_envelope(&damaged, &line, None).unwrap();
    assert_eq!(count_events(&store), 2);
    let error = import_all(&damaged).unwrap_err();
    assert!(
        error.contains("database disk image is malformed") && error.contains("write refused"),
        "{error}"
    );
    let attention = std::fs::read_to_string(dir.path().join("questions.jsonl")).unwrap();
    assert!(attention.contains("event-store-integrity"));
    let items = crate::attention::project(&attention, &[], "", 0);
    assert_eq!(items.len(), 1);
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
    assert_eq!(retention_class("lead_checkin"), "durable");
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
        "source": "hook", "data": {"session_id": "s1", "reviewer": "r1", "node": "x-1",
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
    let second = append_envelope(&live, &envelope, None).unwrap();
    assert!(
        !second.inserted,
        "a byte-identical retry is an idempotent hit"
    );
    assert_eq!(second.event_id, receipt.event_id);
    assert_eq!(count_events(&store_path(&live)), 1);
    // The other branch of the same identity contract: a row already stored
    // under a requested id, then the SAME id with a DIFFERENT payload, is a
    // refusal, not a silent second row.
    let named = checkin("2026-09-17T12:00:00Z", "x-aaaa", "one payload").to_string();
    append_envelope(&live, &named, Some("evt:requested")).unwrap();
    let count_with_named = count_events(&store_path(&live));
    let colliding = checkin("2026-09-17T12:00:00Z", "x-aaaa", "DIFFERENT").to_string();
    let err = append_envelope(&live, &colliding, Some("evt:requested")).unwrap_err();
    assert!(err.contains("identity collision"), "err: {err}");
    assert_eq!(count_events(&store_path(&live)), count_with_named);
}

#[test]
fn append_refuses_newline_and_bad_scope_and_bad_ts() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    assert!(append_envelope(&live, "{\"a\":1}\n{\"b\":2}", None).is_err());
    let bad_scope = json!({"ts": "2026-09-17T12:00:00Z", "type": "lead_checkin",
        "source": "loop", "data": {"scope": "x-1 ready, two words", "change": "one"}})
    .to_string();
    let err = append_envelope(&live, &bad_scope, None).unwrap_err();
    assert!(err.contains("canonical team scope"), "err: {err}");
    let bad_ts = json!({"ts": "not-a-time", "type": "lead_checkin",
        "source": "loop", "data": {"scope": "x-1", "change": "one"}})
    .to_string();
    let err = append_envelope(&live, &bad_ts, None).unwrap_err();
    assert!(err.contains("RFC3339"), "err: {err}");
    assert!(
        !store_path(&live).exists(),
        "refused writes never create a store"
    );
}

#[test]
fn a_stop_decision_without_scope_is_auditable_for_every_session() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    let visitor = json!({
        "ts": "2026-09-17T12:00:00Z",
        "type": "stop_decision",
        "source": "hook",
        "data": {
            "session_id": "thread-1",
            "raw_identity_candidates": [],
            "turn_id": "turn-1",
            "manifest": "",
            "scope": "",
            "node_id": "",
            "driver": "unknown",
            "continuation_owner": "none",
            "decision": "allow",
            "class": "visitor",
            "correlation_id": "stop:thread-1:turn-1",
            "harness_output_contract": "empty"
        }
    })
    .to_string();

    let parsed = map_row(&visitor, 0);
    assert_eq!(parsed.3.as_deref(), Some(""));
    assert_eq!(parsed.4, None);
    append_envelope(&live, &visitor, None).unwrap();
    assert_eq!(count_type(&store_path(&live), "stop_decision"), 1);

    // A session a manifest does not yet name - a fresh successor
    // whose only manifest is its predecessor's - journals the same way. The
    // correlated row is what lead admission reads; no manifest needed.
    let successor = json!({
        "ts": "2026-09-17T12:00:00Z",
        "type": "stop_decision",
        "source": "hook",
        "data": {
            "session_id": "thread-1",
            "raw_identity_candidates": [],
            "turn_id": "turn-2",
            "manifest": "/private/leads/x-aaaa.md",
            "scope": "",
            "node_id": "",
            "driver": "lead",
            "continuation_owner": "visitor",
            "decision": "allow",
            "class": "foreign-manifest",
            "correlation_id": "stop:thread-1:turn-2",
            "harness_output_contract": "empty"
        }
    })
    .to_string();
    append_envelope(&live, &successor, None).unwrap();
    assert_eq!(count_type(&store_path(&live), "stop_decision"), 2);

    // A NON-empty scope still validates against the canonical form.
    let bad_scope = json!({
        "ts": "2026-09-17T12:00:00Z",
        "type": "stop_decision",
        "source": "hook",
        "data": {
            "session_id": "thread-1",
            "raw_identity_candidates": [],
            "turn_id": "turn-3",
            "manifest": "",
            "scope": "ready no build, idea",
            "node_id": "",
            "driver": "lead",
            "continuation_owner": "none",
            "decision": "allow",
            "class": "visitor",
            "correlation_id": "stop:thread-1:turn-3",
            "harness_output_contract": "empty"
        }
    })
    .to_string();
    let error = append_envelope(&live, &bad_scope, None).unwrap_err();
    assert!(error.contains("canonical team scope"), "error: {error}");
}

#[test]
fn append_requires_type_source_data_object() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    for (bad, needle) in [
        (
            r#"{"ts": "2026-09-17T12:00:00Z", "source": "s", "data": {}}"#,
            "missing required field: type",
        ),
        (
            r#"{"ts": "2026-09-17T12:00:00Z", "type": "", "source": "hook", "data": {}}"#,
            "unknown event type:",
        ),
        (
            r#"{"ts": "2026-09-17T12:00:00Z", "type": "t", "data": {}}"#,
            "missing required field: source",
        ),
        (
            r#"{"ts": "2026-09-17T12:00:00Z", "type": "claim_released", "source": "hook", "data": [1]}"#,
            "event data must be an object",
        ),
        ("[1,2]", "envelope is not a JSON object"),
    ] {
        let err = append_envelope(&live, bad, None).unwrap_err();
        assert!(err.contains(needle), "{bad}: {err}");
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
            "source": "hook", "data": {"reviewer": "r1", "session_id": "s1",
            "node": node, "head_sha": "h1", "verdict": verdict}})
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
    let text = journal_text(&live, &["lead_checkin"]);
    assert_eq!(
        text,
        format!("{}\n{tail}\n", older.clone()),
        "rotated history first with the unasked type gone, live tail verbatim"
    );
    // The read never syncs: reading twice is stable.
    assert_eq!(journal_text(&live, &["lead_checkin"]), text);
    // The no-filter branch of the same surface: every committed row,
    // asked type or not.
    let unfiltered = journal_text(&live, &[]);
    assert!(unfiltered.contains("lead_checkin"));
    assert!(unfiltered.contains("unwanted_kind"));
}

#[test]
fn journal_text_checked_without_store_reads_a_rotation_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    let rotated = dir.path().join("events.jsonl.1");
    // No store beside the journal: the rotation must yield its own bytes,
    // never the live file live_journal folds it into.
    std::fs::write(&live, "{\"n\":1}\n").unwrap();
    std::fs::write(&rotated, "{\"n\":2}\n").unwrap();
    let text = journal_text_checked(&rotated, &EventQuery::of_types(&[])).unwrap();
    assert_eq!(
        text, "{\"n\":2}\n",
        "the rotation's own bytes, not the live file"
    );
    // Same branch, absent base journal: the read is empty and creates no
    // store.
    let absent = dir.path().join("absent.jsonl");
    assert_eq!(
        journal_text_checked(&absent, &EventQuery::of_types(&[])).unwrap(),
        ""
    );
    assert!(!store_path(&absent).exists());
}

#[test]
fn journal_text_reads_committed_rows_in_commit_order() {
    // AC1-HP: an imported row stays ahead of a store-only commit, and the
    // two rows share one second so equal timestamps must keep append order.
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    let a = checkin("2026-09-17T12:00:00Z", "x-aaaa", "imported");
    append(&live, &[a]);
    sync(&live).unwrap();
    let b = checkin("2026-09-17T12:00:00Z", "x-aaaa", "store-only");
    append_envelope(&live, &b.to_string(), None).unwrap();
    let text = journal_text(&live, &["lead_checkin"]);
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
    let err = journal_text_checked(&live, &EventQuery::of_types(&["lead_checkin"])).unwrap_err();
    assert!(err.contains("events.db"), "err names the store path: {err}");
    assert_eq!(
        journal_text(&live, &["lead_checkin"]),
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

#[test]
fn journal_text_checked_cursor_fast_path_matches_the_full_scan() {
    // AC1-HP: a fresh cursor means the committed prefix is probed never; the
    // text equals committed rows plus the un-ingested tail, exactly what a
    // full scan returns.
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    let a = checkin("2026-09-17T12:00:00Z", "x-aaaa", "row-a");
    let b = checkin("2026-09-17T12:01:00Z", "x-aaaa", "row-b");
    append(&live, &[a, b]);
    sync(&live).unwrap();
    append(
        &live,
        &[checkin("2026-09-17T12:02:00Z", "x-aaaa", "raw-tail")],
    );
    let text = journal_text_checked(&live, &EventQuery::of_types(&[])).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "A, B, C: {text}");
    assert!(
        lines[0].contains("\"row-a\"")
            && lines[1].contains("\"row-b\"")
            && lines[2].contains("\"raw-tail\""),
        "committed rows then the tail, no dupes: {text}"
    );
}

#[test]
fn journal_text_checked_stale_cursor_falls_back_to_the_full_scan() {
    // AC1-EDGE: the head line changed under the cursor, so the prefix is not
    // committed knowledge and the full scan must run.
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonline");
    append(&live, &[checkin("2026-09-17T12:00:00Z", "x-aaaa", "row-a")]);
    sync(&live).unwrap();
    // Rewrite the same inode with different content: head hash no longer
    // matches, so the cursor is stale.
    let stale = checkin("2026-09-17T12:00:00Z", "x-bbbb", "row-x");
    std::fs::write(&live, format!("{stale}\n")).unwrap();
    append(&live, &[checkin("2026-09-17T12:01:00Z", "x-aaaa", "row-y")]);
    let text = journal_text_checked(&live, &EventQuery::of_types(&[])).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    // The store is authoritative: the committed row-a survives the live-file
    // rewrite; the stale-cursor fallback appends the two unseen lines.
    assert_eq!(lines.len(), 3, "committed A, then X, Y: {text}");
    assert!(
        lines[0].contains("\"row-a\"")
            && lines[1].contains("\"row-x\"")
            && lines[2].contains("\"row-y\""),
        "stale cursor still reads correctly: {text}"
    );
}

#[test]
fn journal_text_checked_fast_path_stays_bounded_on_a_huge_journal() {
    // The perf guard: a cursor-current journal reads in bounded time because
    // the probe loop sees the tail alone; a regression to the full scan pays
    // 200k hash+EXISTS rounds and blows the bound.
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("events.jsonl");
    let a = checkin("2026-09-17T12:00:00Z", "x-aaaa", "row-a");
    append(&live, &[a]);
    sync(&live).unwrap();
    let filler = checkin("2026-09-17T12:01:00Z", "x-aaaa", "filler");
    {
        let mut fh = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&live)
            .unwrap();
        for i in 0..200_000 {
            let mut row = filler.clone();
            row["data"]["change"] = json!(format!("filler-{i}"));
            writeln!(fh, "{row}").unwrap();
        }
    }
    // Ingest everything so the cursor sits at the true file end: the probe
    // loop then sees the one tail line alone. A regression to the full scan
    // pays 200k hash+EXISTS rounds and blows the bound.
    sync(&live).unwrap();
    append(
        &live,
        &[checkin("2026-09-17T12:02:00Z", "x-aaaa", "raw-tail")],
    );
    let started = std::time::Instant::now();
    let text = journal_text_checked(&live, &EventQuery::of_types(&[])).unwrap();
    let elapsed = started.elapsed();
    let n = text.lines().count();
    assert!(
        n >= 200_002,
        "every committed row plus the tail still reads: {n} lines"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "the cursor fast path keeps the read bounded: {elapsed:?}"
    );
}

#[test]
fn a_filtered_read_sorts_seqs_not_lines_and_keeps_limit_order() {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("events.jsonl");
    append(
        &journal,
        &[
            checkin("2026-10-07T00:00:03Z", "s", "first"),
            checkin("2026-10-07T00:00:01Z", "s", "second"),
            checkin("2026-10-07T00:00:02Z", "s", "third"),
        ],
    );
    sync(&journal).unwrap();
    let mut q = EventQuery::of_types(&["lead_checkin"]);
    let (sql, args) = q.build_sql(false);
    let conn = open_read(&store_path(&journal)).unwrap();
    let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let plan: Vec<String> = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap()
        .query_map(refs.as_slice(), |r| r.get::<_, String>(3))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(
        !plan.iter().any(|step| step.contains("TEMP B-TREE")),
        "{plan:?}"
    );
    q.limit = Some(2);
    let text = journal_text_checked(&journal, &q).unwrap();
    let kept: Vec<&str> = ["first", "second", "third"]
        .into_iter()
        .filter(|c| text.contains(c))
        .collect();
    assert_eq!(kept, ["first", "second"], "{text}");
}

#[test]
fn a_lost_row_is_dead_lettered_by_type_and_busy_locks_are_matched() {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("events.jsonl");
    let store = store_path(&journal);
    // Junk in the store file fails the open's schema probe, so the commit
    // errors and the row is lost for real.
    std::fs::write(&store, b"junk".repeat(50)).unwrap();
    let envelope = json!({
        "ts": "2026-10-07T00:00:00Z",
        "type": "claim_released",
        "source": "fno-loop",
        "data": {
            "session_id": "s-1",
            "key": "node:x-1:claude:s-1",
            "holder": "s-1",
            "pid": 4242,
            "host": "test-host",
            "acquired_at": "2026-10-06T00:00:00Z",
            "duration_held_ms": 60000,
        },
    })
    .to_string();
    let error = append_envelope(&journal, &envelope, None).unwrap_err();
    // The open's schema probe fails NOTADB, so the diagnostic is the bare
    // named() string, not the corrupt-image form.
    assert!(error.contains("file is not a database"), "{error}");

    let sidecar = PathBuf::from(format!("{}.lost.jsonl", store.display()));
    let line = std::fs::read_to_string(&sidecar).unwrap();
    assert!(line.contains("\"type\":\"claim_released\""), "{line}");

    assert!(lock_busy(".../events.db: database is locked"));
    assert!(!lock_busy(".../events.db: file is not a database"));
    assert!(!lock_busy(""));
}

mod coverage;
mod observation;
