//! The durable event store: one SQLite database beside each journal.
//!
//! A durable row's journal keeps one rotation generation, so history died at
//! every 8 MiB rename. This module is the record: [`sync`] ingests the
//! complete lines of the rotated file and then the live file into
//! `<journal-stem>.db` before anything reads history or rotates, keyed by the
//! sha256 of the line so replays, gc rewrites and mirrored rows dedupe.
//!
//! The journal stays the write path; the store is only ever filled from
//! complete journal lines. Ephemeral sibling journals are refused: the
//! retention schema already declares those rows disposable, and the store
//! does not overrule that.
//!
//! ponytail: sync reads a whole file into memory, so one sync's memory scales
//! with the file size. Bounded in practice by the rotation threshold; only a
//! long deferred-rotation window (broken store) grows past it, and that window
//! already screams on stderr per emit. Upgrade path: stream from the cursor
//! offset with a BufReader and carry the partial tail line.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{params, Connection, Transaction, TransactionBehavior};
use serde::Serialize;
use sha2::{Digest, Sha256};

/// Durable rows older than this leave the store (locked decision 4: one
/// horizon constant; a per-type class if the size ever bites).
const STORE_RETENTION_DAYS: i64 = 30;

const DAY_MS: i64 = 86_400_000;

/// One receipt per [`sync`]: what the store look looks like after the ingest.
#[derive(Debug, Serialize)]
pub(crate) struct SyncReceipt {
    pub store: PathBuf,
    pub ingested: u64,
    pub corrupt: u64,
    pub read_bytes: u64,
}

#[derive(Default)]
struct FileTally {
    ingested: u64,
    corrupt: u64,
    read_bytes: u64,
}

/// The live journal a path belongs to: the generation suffix stripped and
/// symlinks resolved, so `events.jsonl` and `events.jsonl.1` name one store.
pub(crate) fn live_journal(journal: &Path) -> PathBuf {
    let resolved = std::fs::canonicalize(journal).unwrap_or_else(|_| journal.to_path_buf());
    let mut stem = resolved
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if let Some(base) = strip_rotation_suffix(&stem) {
        stem = base;
    }
    resolved.with_file_name(stem)
}

/// The store beside a journal: the live journal's `.jsonl` stem plus `.db`.
pub(crate) fn store_path(journal: &Path) -> PathBuf {
    let live = live_journal(journal);
    let stem = live
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem = stem.strip_suffix(".jsonl").unwrap_or(&stem);
    live.with_file_name(format!("{stem}.db"))
}

fn strip_rotation_suffix(name: &str) -> Option<String> {
    let (base, digits) = name.rsplit_once('.')?;
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())).then(|| base.to_string())
}

/// Ingest the rotated generation and then the live journal into the store.
/// The inode cursor makes the rotation free: the file at `.1` is the inode
/// that was live, so its cursor resumes where the live file left off.
pub(crate) fn sync(live: &Path) -> Result<SyncReceipt, String> {
    let ephemeral = live
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.contains(crate::events::EPHEMERAL_SUFFIX));
    if ephemeral {
        return Err(format!(
            "{}: ephemeral journals are never ingested",
            live.display()
        ));
    }
    let store = store_path(live);
    if let Some(parent) = store.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", store.display()))?;
    }
    let mut conn = open_store(&store)?;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| format!("{}: {e}", store.display()))?;
    let rotated = ingest_file(&tx, &crate::events::rotated_path(live), now_ms)?;
    let live_tally = ingest_file(&tx, live, now_ms)?;
    tx.commit()
        .map_err(|e| format!("{}: {e}", store.display()))?;
    maybe_prune(&mut conn, now_ms)?;
    Ok(SyncReceipt {
        store,
        ingested: rotated.ingested + live_tally.ingested,
        corrupt: rotated.corrupt + live_tally.corrupt,
        read_bytes: rotated.read_bytes + live_tally.read_bytes,
    })
}

/// Open (creating if needed) the store with the backlog shadow's pragmas:
/// WAL so concurrent history readers never block the ingest writer, FULL so
/// an acknowledged ingest survives a crash.
fn open_store(store: &Path) -> Result<Connection, String> {
    let conn = Connection::open(store).map_err(|e| format!("{}: {e}", store.display()))?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|e| e.to_string())?;
    conn.execute_batch(&format!(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=FULL;
         CREATE TABLE IF NOT EXISTS events (
             row_hash BLOB PRIMARY KEY NOT NULL,
             ts_ms INTEGER NOT NULL,
             type TEXT NOT NULL,
             source TEXT NOT NULL,
             scope TEXT,
             reject_reason TEXT,
             line TEXT NOT NULL
         ) WITHOUT ROWID;
         CREATE INDEX IF NOT EXISTS events_scope_type_ts ON events(scope, type, ts_ms);
         CREATE TABLE IF NOT EXISTS ingest_cursor (
             dev INTEGER NOT NULL,
             ino INTEGER NOT NULL,
             head_hash BLOB NOT NULL,
             \"offset\" INTEGER NOT NULL,
             path TEXT NOT NULL,
             updated_ms INTEGER NOT NULL,
             PRIMARY KEY (dev, ino)
         );
         CREATE TABLE IF NOT EXISTS events_meta (
             key TEXT PRIMARY KEY,
             value TEXT NOT NULL
         );"
    ))
    .map_err(|e| format!("{}: {e}", store.display()))?;
    Ok(conn)
}

/// Read-only handle for history readers; a failure names the store path.
pub(crate) fn open_read(store: &Path) -> Result<Connection, String> {
    let conn = Connection::open_with_flags(store, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("{}: {e}", store.display()))?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|e| e.to_string())?;
    Ok(conn)
}

fn ingest_file(tx: &Transaction, path: &Path, now_ms: i64) -> Result<FileTally, String> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(FileTally::default()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    // Offsets are BYTE offsets into the raw file, never into a lossy string:
    // a conversion that resizes bytes would desync the stored cursor.
    let head_hash: Vec<u8> = {
        let first = bytes.split(|&b| b == b'\n').next().unwrap_or(&[]);
        Sha256::digest(first).to_vec()
    };
    let (dev, ino, len) = (meta.dev() as i64, meta.ino() as i64, meta.len());
    let resume: Option<(Vec<u8>, i64)> = tx
        .query_row(
            "SELECT head_hash, \"offset\" FROM ingest_cursor WHERE dev = ?1 AND ino = ?2",
            params![dev, ino],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();
    // Resume only when the head line still hashes equal AND the file has not
    // shrunk under the cursor; anything else re-reads from zero and lets
    // row_hash dedupe the overlap.
    let mut start = 0usize;
    if let Some((head, offset)) = resume {
        if head == head_hash
            && offset >= 0
            && (offset as u64) <= len
            && (offset as usize) <= bytes.len()
        {
            start = offset as usize;
        }
    }
    // Only complete lines: a tail without its newline belongs to the next sync.
    let complete_end = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let mut tally = FileTally::default();
    for line_bytes in bytes[start..complete_end].split(|&b| b == b'\n') {
        let line_bytes = line_bytes.strip_suffix(b"\r").unwrap_or(line_bytes);
        if line_bytes.is_empty() {
            continue;
        }
        tally.read_bytes += line_bytes.len() as u64 + 1;
        let row_hash = Sha256::digest(line_bytes).to_vec();
        // An invalid-UTF-8 line is stored as evidence; the byte cursor above
        // stays exact regardless.
        let line = String::from_utf8_lossy(line_bytes).into_owned();
        let (ts_ms, ty, source, scope, reject) = map_row(&line, now_ms);
        if reject == Some("corrupt json") {
            tally.corrupt += 1;
        }
        let inserted = tx
            .execute(
                "INSERT OR IGNORE INTO events
                     (row_hash, ts_ms, type, source, scope, reject_reason, line)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![row_hash, ts_ms, ty, source, scope, reject, line],
            )
            .map_err(|e| format!("{}: {e}", path.display()))?;
        tally.ingested += inserted as u64;
    }
    tx.execute(
        "INSERT INTO ingest_cursor (dev, ino, head_hash, \"offset\", path, updated_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(dev, ino) DO UPDATE SET
             head_hash = ?3, \"offset\" = ?4, path = ?5, updated_ms = ?6",
        params![
            dev,
            ino,
            head_hash,
            complete_end as i64,
            path.display().to_string(),
            now_ms
        ],
    )
    .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(tally)
}

/// Map one journal line to its columns. No line is ever dropped: the first
/// failure lands in `reject_reason` and the row stays queryable verbatim.
fn map_row(line: &str, now_ms: i64) -> (i64, String, String, Option<String>, Option<&'static str>) {
    let reject = |ts_ms: i64,
                  ty: &str,
                  source: &str,
                  reason: &'static str|
     -> (i64, String, String, Option<String>, Option<&'static str>) {
        (
            ts_ms,
            ty.to_string(),
            source.to_string(),
            None,
            Some(reason),
        )
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return reject(now_ms, "", "", "corrupt json");
    };
    let Some(obj) = value.as_object() else {
        return reject(now_ms, "", "", "corrupt json");
    };
    let ty = obj
        .get("type")
        .and_then(|t| t.as_str())
        .or_else(|| obj.get("kind").and_then(|k| k.as_str()));
    let Some(ty) = ty else {
        return reject(now_ms, "", "", "no type");
    };
    let source = obj.get("source").and_then(|s| s.as_str()).unwrap_or("");
    let ts_str = obj.get("ts").and_then(|t| t.as_str());
    let ts_ms = match ts_str.and_then(parse_rfc3339_ms) {
        Some(ms) => ms,
        None => return reject(now_ms, ty, source, "unparseable ts"),
    };
    let scope = obj
        .get("data")
        .and_then(|d| d.get("scope"))
        .and_then(|s| s.as_str());
    if let Some(s) = scope {
        if !is_canonical_crown_scope(s) {
            return reject(ts_ms, ty, source, "scope is not a canonical crown scope");
        }
    }
    (
        ts_ms,
        ty.to_string(),
        source.to_string(),
        scope.map(str::to_string),
        None,
    )
}

/// A canonical crown scope is comma-joined sorted members, none blank, none
/// carrying whitespace (that is how the rendered-board corruption of
/// 2026-09-14 is detectable at write time).
fn is_canonical_crown_scope(s: &str) -> bool {
    !s.is_empty()
        && crate::territory::canonical_scope(s) == s
        && s.split(',')
            .all(|m| !m.is_empty() && !m.chars().any(char::is_whitespace))
}

fn parse_rfc3339_ms(ts: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

/// Once a day: rows older than [`STORE_RETENTION_DAYS`] and cursors not
/// updated in that window leave the store.
fn maybe_prune(conn: &mut Connection, now_ms: i64) -> Result<(), String> {
    let last: i64 = conn
        .query_row(
            "SELECT value FROM events_meta WHERE key = 'last_prune_ms'",
            [],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if now_ms.saturating_sub(last) < DAY_MS {
        return Ok(());
    }
    let cutoff = now_ms.saturating_sub(STORE_RETENTION_DAYS * DAY_MS);
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| e.to_string())?;
    tx.execute("DELETE FROM events WHERE ts_ms < ?1", params![cutoff])
        .map_err(|e| e.to_string())?;
    tx.execute(
        "DELETE FROM ingest_cursor WHERE updated_ms < ?1",
        params![cutoff],
    )
    .map_err(|e| e.to_string())?;
    tx.execute(
        "INSERT INTO events_meta (key, value) VALUES ('last_prune_ms', ?1)
         ON CONFLICT(key) DO UPDATE SET value = ?1",
        params![now_ms.to_string()],
    )
    .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn journal(dir: &Path, name: &str) -> PathBuf {
        dir.join(name)
    }

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

    #[test]
    fn store_path_strips_generation_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let live = journal(dir.path(), "events.jsonl");
        assert_eq!(store_path(&live), dir.path().join("events.db"));
        let rotated = journal(dir.path(), "events.jsonl.1");
        assert_eq!(store_path(&rotated), dir.path().join("events.db"));
        let named = journal(dir.path(), "global.jsonl");
        assert_eq!(store_path(&named), dir.path().join("global.db"));
    }

    #[test]
    fn sync_ingests_both_generations_and_second_sync_is_zero() {
        let dir = tempfile::tempdir().unwrap();
        let live = journal(dir.path(), "events.jsonl");
        append(
            &live,
            &[checkin("2026-09-10T12:00:00Z", "x-aaaa", "newest")],
        );
        let rotated = journal(dir.path(), "events.jsonl.1");
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
        // AC2-HP's store half: once .1 is replaced by the next generation,
        // the rows only the store holds are still there.
        let dir = tempfile::tempdir().unwrap();
        let live = journal(dir.path(), "events.jsonl");
        let rotated = journal(dir.path(), "events.jsonl.1");
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
        assert_eq!(
            count_checkins(&receipt.store, "x-aaaa"),
            3,
            "every generation survives the rotation"
        );
    }

    fn count_checkins(store: &Path, scope: &str) -> i64 {
        open_read(store)
            .unwrap()
            .query_row(
                "SELECT count(*) FROM events WHERE type = 'reign_checkin' AND scope = ?1",
                params![scope],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn gc_rewrite_dedupes_by_row_hash() {
        // AC1-ERR: a rewrite under a new inode with the same rows plus one.
        let dir = tempfile::tempdir().unwrap();
        let live = journal(dir.path(), "events.jsonl");
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
        // AC1-EDGE: nothing drops; the first failure names the cause.
        let dir = tempfile::tempdir().unwrap();
        let live = journal(dir.path(), "events.jsonl");
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
        let live = journal(dir.path(), "events.jsonl");
        append(&live, &[checkin("2026-09-10T08:00:00Z", "x-aaaa", "clean")]);
        let receipt = sync(&live).unwrap();
        assert_eq!(count_checkins(&receipt.store, "x-aaaa"), 1);
    }

    #[test]
    fn ephemeral_journal_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let live = journal(dir.path(), "events.jsonl.ephemeral");
        append(&live, &[checkin("2026-09-10T08:00:00Z", "x-aaaa", "gauge")]);
        let err = sync(&live).unwrap_err();
        assert!(err.contains("ephemeral"), "err: {err}");
        assert!(!store_path(&live).exists(), "no store was created");
    }

    #[test]
    fn prune_drops_rows_past_the_retention_horizon() {
        let dir = tempfile::tempdir().unwrap();
        let live = journal(dir.path(), "events.jsonl");
        append(
            &live,
            &[
                checkin("2026-05-01T08:00:00Z", "x-aaaa", "ancient"),
                checkin("2026-09-10T08:00:00Z", "x-aaaa", "fresh"),
            ],
        );
        let receipt = sync(&live).unwrap();
        // The daily prune is keyed off events_meta; force the horizon to fire
        // by clearing the stamp so a second sync prunes immediately.
        let conn = open_read(&receipt.store).unwrap();
        drop(conn);
        let writable = Connection::open(&receipt.store).unwrap();
        writable
            .execute("DELETE FROM events_meta WHERE key = 'last_prune_ms'", [])
            .unwrap();
        drop(writable);
        sync(&live).unwrap();
        let store = receipt.store;
        assert_eq!(count_events(&store), 1);
        assert_eq!(count_checkins(&store, "x-aaaa"), 1);
    }
}
