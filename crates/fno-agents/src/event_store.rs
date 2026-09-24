//! The authoritative runtime event store: one SQLite database beside each journal.
//!
//! A durable row's journal keeps one rotation generation, so history died at
//! every 8 MiB rename. This module is the record: [`sync`] ingests the
//! complete lines of the rotated file and then the live file into
//! `<journal-stem>.db` before anything reads history or rotates, keyed by the
//! sha256 of the line so replays, gc rewrites and mirrored rows dedupe.
//!
//! Schema v2 (`PRAGMA user_version` 2) makes the store ordered and
//! identity-aware: every row carries a commit-order `seq`, a stable
//! `event_id`, a `retention_class`, and the identity columns gates query
//! (`session_id`, `node_id`, `pr_number`, `head_sha`, `repo`). A v1 database
//! (the pre-`seq` shape) migrates in place, in one transaction, oldest-first,
//! with `event_id = legacy:<sha256(raw line)>`; a crash mid-migration rolls
//! back and retries import zero duplicates.
//!
//! Retention: `durable` and `gate` rows never auto-expire. Only `ephemeral`
//! rows leave the store, after [`MINIMUM_EPHEMERAL_TTL_HOURS`]. No line is
//! ever dropped at import: the first parse failure lands in `reject_reason`
//! and the row stays queryable verbatim.
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

/// The store schema this crate writes. v1 was the unordered
/// `row_hash`-keyed table; v2 adds `seq`, `event_id`, `retention_class`,
/// and identity columns.
pub const SCHEMA_VERSION: i64 = 2;

/// Sibling journal suffix for ephemeral-class rows. The Python
/// `fno.events` module declares the same string; a parity test
/// (`cli/tests/events/test_ephemeral_set_parity.py`) holds the two equal so
/// both languages write the same sibling file.
pub const EPHEMERAL_SUFFIX: &str = ".ephemeral";

/// Event types the schema declares `retention: ephemeral`. Kept equal to
/// `cli/src/fno/events/schema.yaml` by the same parity test; flip the class in
/// both places or the two write boundaries route differently.
pub const EPHEMERAL_EVENT_TYPES: &[&str] = &[
    "claim_acquired",
    "claim_clock_skew_rejected",
    "claim_force_overridden",
    "claim_idempotent_reacquired",
    "claim_rebound",
    "claim_refreshed",
    "claim_released",
    "claim_stale_reclaimed",
    "graph_tx_conflict",
    "human_touch",
    "mux_pane_counters",
    "orphan_reap_sweep",
    "single_flight_gate",
];

/// Event types the schema declares `retention: gate`: rows merge gates read
/// as positive evidence. They never auto-expire.
pub const GATE_EVENT_TYPES: &[&str] = &["review_attestation", "review_coverage"];

/// Retention floor for `ephemeral` rows (`schema.yaml`:
/// `retention.minimum_ephemeral_ttl_hours`).
pub const MINIMUM_EPHEMERAL_TTL_HOURS: i64 = 672;

const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 86_400_000;

/// Whether an event kind belongs to the schema-declared ephemeral class.
pub fn is_ephemeral_event(kind: &str) -> bool {
    EPHEMERAL_EVENT_TYPES.contains(&kind)
}

/// Whether an event kind belongs to the schema-declared gate class.
pub fn is_gate_event(kind: &str) -> bool {
    GATE_EVENT_TYPES.contains(&kind)
}

/// The retention class a kind belongs to: `ephemeral`, `gate`, or `durable`
/// (the schema default).
pub fn retention_class(kind: &str) -> &'static str {
    if is_ephemeral_event(kind) {
        "ephemeral"
    } else if is_gate_event(kind) {
        "gate"
    } else {
        "durable"
    }
}

/// One receipt per [`sync`]: what the store looks like after the ingest.
#[derive(Debug, Serialize)]
pub struct SyncReceipt {
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
pub fn live_journal(journal: &Path) -> PathBuf {
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
pub fn store_path(journal: &Path) -> PathBuf {
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
pub fn sync(live: &Path) -> Result<SyncReceipt, String> {
    let ephemeral = live
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.contains(EPHEMERAL_SUFFIX));
    if ephemeral {
        return Err(format!(
            "{}: ephemeral journals are never ingested",
            live.display()
        ));
    }
    sync_sources(live, &[&rotated_generation(live), live])
}

/// The `.1` generation beside a live journal (never created, just named).
fn rotated_generation(live: &Path) -> PathBuf {
    let mut s = live.as_os_str().to_os_string();
    s.push(".1");
    PathBuf::from(s)
}

/// The sibling journal an ephemeral-class row routes to (same directory,
/// same stem, the [`EPHEMERAL_SUFFIX`] tail).
fn ephemeral_sibling(live: &Path) -> PathBuf {
    let mut s = live.as_os_str().to_os_string();
    s.push(EPHEMERAL_SUFFIX);
    PathBuf::from(s)
}

/// Ingest every generation of one journal family - rotated durable, live
/// durable, then the ephemeral sibling - into one store, in one transaction.
/// History import is oldest-first by generation, and the `row_hash` key makes
/// a retried import after a crash insert zero duplicates.
pub fn import_all(live: &Path) -> Result<SyncReceipt, String> {
    // Every retained generation imports, oldest first, then the live file,
    // then the ephemeral sibling - the same order the pre-store segment walk
    // concatenated, so commit-order rows read the same as the old concat.
    let mut generations: Vec<(u64, PathBuf)> = Vec::new();
    if let (Some(dir), Some(name)) = (live.parent(), live.file_name()) {
        let name = name.to_string_lossy();
        let prefix = format!("{name}.");
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let candidate = entry.file_name().to_string_lossy().into_owned();
                if let Some(gen) = candidate
                    .strip_prefix(&prefix)
                    .and_then(|g| g.parse::<u64>().ok())
                {
                    generations.push((gen, dir.join(&candidate)));
                }
            }
        }
    }
    generations.sort_by_key(|(gen, _)| *gen);
    let mut sources: Vec<PathBuf> = generations.into_iter().map(|(_, p)| p).collect();
    sources.push(live.to_path_buf());
    sources.push(ephemeral_sibling(live));
    let refs: Vec<&Path> = sources.iter().map(|p| p.as_path()).collect();
    sync_sources(live, &refs)
}

fn sync_sources(live: &Path, sources: &[&Path]) -> Result<SyncReceipt, String> {
    let store = store_path(live);
    if let Some(parent) = store.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", store.display()))?;
    }
    let mut conn = open_store(&store)?;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| format!("{}: {e}", store.display()))?;
    let mut total = FileTally::default();
    for source in sources {
        let tally = import_file(&tx, source, now_ms)?;
        total.ingested += tally.ingested;
        total.corrupt += tally.corrupt;
        total.read_bytes += tally.read_bytes;
    }
    tx.commit()
        .map_err(|e| format!("{}: {e}", store.display()))?;
    prune(&mut conn, now_ms)?;
    Ok(SyncReceipt {
        store,
        ingested: total.ingested,
        corrupt: total.corrupt,
        read_bytes: total.read_bytes,
    })
}

/// Open (creating if needed) the store with the backlog shadow's pragmas:
/// WAL so concurrent history readers never block the ingest writer, FULL so
/// an acknowledged ingest survives a crash. The schema is ensured (v2
/// created, or v1 migrated) before the connection is handed out.
fn open_store(store: &Path) -> Result<Connection, String> {
    let mut conn = Connection::open(store).map_err(|e| format!("{}: {e}", store.display()))?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|e| e.to_string())?;
    configure_store_connection(&conn, store)?;
    ensure_schema(&mut conn, store)?;
    Ok(conn)
}

fn configure_store_connection(conn: &Connection, store: &Path) -> Result<(), String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let configured = (|| -> rusqlite::Result<()> {
            let journal_mode: String =
                conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
            if !journal_mode.eq_ignore_ascii_case("wal") {
                conn.execute_batch("PRAGMA journal_mode=WAL;")?;
            }
            let synchronous: i64 = conn.query_row("PRAGMA synchronous", [], |row| row.get(0))?;
            if synchronous != 2 {
                conn.execute_batch("PRAGMA synchronous=FULL;")?;
            }
            Ok(())
        })();
        match configured {
            Ok(()) => return Ok(()),
            Err(error)
                if matches!(
                    error.sqlite_error_code(),
                    Some(
                        rusqlite::ffi::ErrorCode::DatabaseBusy
                            | rusqlite::ffi::ErrorCode::DatabaseLocked
                    )
                ) && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(format!("{}: {error}", store.display())),
        }
    }
}

/// Read-only handle for history readers; a failure names the store path.
pub fn open_read(store: &Path) -> Result<Connection, String> {
    let conn = Connection::open_with_flags(store, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("{}: {e}", store.display()))?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|e| e.to_string())?;
    refuse_newer_schema(&conn, store)?;
    Ok(conn)
}

fn refuse_newer_schema(conn: &Connection, store: &Path) -> Result<i64, String> {
    let current = conn
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .map_err(|e| format!("{}: schema version unreadable: {e}", store.display()))?;
    if current > SCHEMA_VERSION {
        return Err(format!(
            "events store schema v{current} is newer than this build understands (v{SCHEMA_VERSION}); upgrade fno before touching {}",
            store.display()
        ));
    }
    Ok(current)
}

const EVENTS_V2_COLUMNS: &str = "(\
        seq INTEGER PRIMARY KEY, \
        event_id TEXT UNIQUE NOT NULL, \
        row_hash BLOB UNIQUE NOT NULL, \
        ts_ms INTEGER NOT NULL, \
        type TEXT NOT NULL, \
        source TEXT NOT NULL, \
        scope TEXT, \
        retention_class TEXT NOT NULL DEFAULT 'durable', \
        session_id TEXT, \
        node_id TEXT, \
        pr_number INTEGER, \
        head_sha TEXT, \
        repo TEXT, \
        reject_reason TEXT, \
        line TEXT NOT NULL\
    )";

const EVENTS_V2_INDEXES: &str = "\
CREATE INDEX IF NOT EXISTS events_scope_type_ts ON events(scope, type, ts_ms);
CREATE INDEX IF NOT EXISTS events_type_ts ON events(type, ts_ms);
CREATE INDEX IF NOT EXISTS events_session_id ON events(session_id) WHERE session_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS events_node_id ON events(node_id) WHERE node_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS events_pr_head ON events(pr_number, head_sha);
CREATE INDEX IF NOT EXISTS events_retention_ts ON events(retention_class, ts_ms);";

/// Ensure the store speaks schema v2: a fresh store is created in the v2
/// shape; a v1 store (`events` without an `event_id` column) migrates in
/// place; a v2 store is a no-op. Any failure rolls the transaction back, so
/// `user_version`, row counts, and completion metadata are untouched.
pub fn ensure_schema(conn: &mut Connection, store: &Path) -> Result<(), String> {
    let current = refuse_newer_schema(conn, store)?;
    let already_v2: bool = current >= SCHEMA_VERSION && events_table_has_event_id(conn);
    if already_v2 {
        return Ok(());
    }
    let has_events: bool = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'events'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .unwrap_or(false);
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| format!("{}: migration: {e}", store.display()))?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS ingest_cursor (
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
         );",
    )
    .map_err(|e| format!("{}: migration: {e}", store.display()))?;
    if has_events && events_table_has_event_id(&tx) {
        // v2-shaped table without the version stamp (a crashed first create):
        // finish the bookkeeping, never rebuild.
        create_v2_indexes(&tx)?;
        stamp_v2(&tx)?;
    } else if has_events {
        migrate_v1_to_v2(&tx, store)?;
    } else {
        tx.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS events {EVENTS_V2_COLUMNS};"
        ))
        .map_err(|e| format!("{}: migration: {e}", store.display()))?;
        create_v2_indexes(&tx)?;
        stamp_v2(&tx)?;
    }
    tx.commit()
        .map_err(|e| format!("{}: migration: {e}", store.display()))
}

fn events_table_has_event_id(conn: &Connection) -> bool {
    let Ok(mut stmt) = conn.prepare("PRAGMA table_info(events)") else {
        return false;
    };
    stmt.query_map([], |r| r.get::<_, String>(1))
        .map(|rows| rows.filter_map(|r| r.ok()).any(|name| name == "event_id"))
        .unwrap_or(false)
}

fn create_v2_indexes(tx: &Transaction) -> Result<(), String> {
    tx.execute_batch(EVENTS_V2_INDEXES)
        .map_err(|e| format!("migration indexes: {e}"))
}

fn stamp_v2(tx: &Transaction) -> Result<(), String> {
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)
        .map_err(|e| format!("migration stamp: {e}"))?;
    tx.execute(
        "INSERT INTO events_meta (key, value) VALUES ('schema_version', ?1)
         ON CONFLICT(key) DO UPDATE SET value = ?1",
        params![SCHEMA_VERSION.to_string()],
    )
    .map(|_| ())
    .map_err(|e| format!("migration stamp: {e}"))
}

/// Rebuild v1 rows into the v2 shape, oldest-first, in the caller's open
/// transaction. Every row survives: accepted rows gain their identity
/// columns, and a malformed row lands with its raw bytes in `line` plus a
/// `reject_reason`. The monotonic `seq` is assigned in `ts_ms` order (the
/// journal's own write order within a generation), ties broken by row_hash
/// so the order is deterministic.
fn migrate_v1_to_v2(tx: &Transaction, store: &Path) -> Result<(), String> {
    tx.execute_batch(&format!("CREATE TABLE events_v2 {EVENTS_V2_COLUMNS};"))
        .map_err(|e| format!("{}: migration: {e}", store.display()))?;
    let mut stmt = tx
        .prepare(
            "SELECT row_hash, ts_ms, type, source, scope, reject_reason, line
             FROM events ORDER BY ts_ms, row_hash",
        )
        .map_err(|e| format!("{}: migration: {e}", store.display()))?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, String>(6)?,
            ))
        })
        .map_err(|e| format!("{}: migration: {e}", store.display()))?;
    for row in rows {
        let (row_hash, ts_ms, ty, source, scope, reject, line) =
            row.map_err(|e| format!("{}: migration: {e}", store.display()))?;
        insert_v2_row(
            tx,
            "events_v2",
            &RowInput {
                event_id: format!("legacy:{}", hex(&row_hash)),
                row_hash: row_hash.clone(),
                ts_ms,
                type_: ty,
                source,
                scope,
                reject_reason: reject,
                line,
            },
        )
        .map_err(|e| format!("{}: migration: {e}", store.display()))?;
    }
    drop(stmt);
    tx.execute("DROP TABLE events", [])
        .map_err(|e| format!("{}: migration: {e}", store.display()))?;
    tx.execute("ALTER TABLE events_v2 RENAME TO events", [])
        .map_err(|e| format!("{}: migration: {e}", store.display()))?;
    create_v2_indexes(tx)?;
    stamp_v2(tx)
}

/// One row destined for the v2 `events` table.
struct RowInput {
    event_id: String,
    row_hash: Vec<u8>,
    ts_ms: i64,
    type_: String,
    source: String,
    scope: Option<String>,
    reject_reason: Option<String>,
    line: String,
}

fn insert_v2_row(tx: &Transaction, table: &str, row: &RowInput) -> Result<usize, rusqlite::Error> {
    let id = extract_identity(&row.line);
    tx.execute(
        &format!(
            "INSERT OR IGNORE INTO {table}
             (event_id, row_hash, ts_ms, type, source, scope, retention_class,
              session_id, node_id, pr_number, head_sha, repo, reject_reason, line)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"
        ),
        params![
            row.event_id,
            row.row_hash,
            row.ts_ms,
            row.type_,
            row.source,
            row.scope,
            retention_class(&row.type_),
            id.session_id,
            id.node_id,
            id.pr_number,
            id.head_sha,
            id.repo,
            row.reject_reason,
            row.line,
        ],
    )
}

/// The identity columns gates query, extracted from one canonical envelope's
/// `data` object. Producers have spelled `node` historically (`node_id` is
/// accepted for newer rows) and `pr` for PR numbers; both spellings land.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct EventIdentity {
    pub session_id: Option<String>,
    pub node_id: Option<String>,
    pub pr_number: Option<i64>,
    pub head_sha: Option<String>,
    pub repo: Option<String>,
}

pub fn extract_identity(line: &str) -> EventIdentity {
    let mut id = EventIdentity::default();
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return id;
    };
    let Some(data) = value.get("data") else {
        return id;
    };
    let get_str = |keys: &[&str]| -> Option<String> {
        keys.iter().find_map(|k| {
            data.get(*k)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
    };
    id.session_id = get_str(&["session_id"]);
    id.node_id = get_str(&["node_id", "node"]);
    id.head_sha = get_str(&["head_sha"]);
    id.repo = get_str(&["repo"]);
    id.pr_number = ["pr_number", "pr"].iter().find_map(|k| {
        data.get(*k).and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
    });
    id
}

/// Import one journal file's complete lines into the store. Any resolved
/// source works here - rotated, live, ephemeral sibling, an agents lifecycle
/// journal, or shell-writer fragments - because the `row_hash` key dedupes
/// overlap and the class comes from the event type, never the file.
fn import_file(tx: &Transaction, path: &Path, now_ms: i64) -> Result<FileTally, String> {
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
        if reject.as_deref() == Some("corrupt json") {
            tally.corrupt += 1;
        }
        let inserted = insert_v2_row(
            tx,
            "events",
            &RowInput {
                event_id: format!("legacy:{}", hex(&row_hash)),
                row_hash,
                ts_ms,
                type_: ty,
                source,
                scope,
                reject_reason: reject,
                line,
            },
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
fn map_row(line: &str, now_ms: i64) -> (i64, String, String, Option<String>, Option<String>) {
    let reject = |ts_ms: i64,
                  ty: &str,
                  source: &str,
                  reason: &'static str|
     -> (i64, String, String, Option<String>, Option<String>) {
        (
            ts_ms,
            ty.to_string(),
            source.to_string(),
            None,
            Some(reason.to_string()),
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
    let data = obj.get("data").unwrap_or(&serde_json::Value::Null);
    let scope = data.get("scope").and_then(|s| s.as_str());
    if let Some(s) = scope {
        if !is_valid_event_scope(ty, data, s) {
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
        && canonical_scope(s) == s
        && s.split(',')
            .all(|m| !m.is_empty() && !m.chars().any(char::is_whitespace))
}

fn is_valid_event_scope(event_type: &str, data: &serde_json::Value, scope: &str) -> bool {
    if !scope.is_empty() {
        return is_canonical_crown_scope(scope);
    }
    // An unowned visitor Stop has no crown scope but remains an auditable event.
    event_type == "stop_decision"
        && data.get("class").and_then(serde_json::Value::as_str) == Some("visitor")
        && data.get("decision").and_then(serde_json::Value::as_str) == Some("allow")
        && data
            .get("continuation_owner")
            .and_then(serde_json::Value::as_str)
            == Some("none")
        && data.get("manifest").and_then(serde_json::Value::as_str) == Some("")
        && data.get("node_id").and_then(serde_json::Value::as_str) == Some("")
}

/// The canonical comma-joined form of a raw scope spelling: members split on
/// commas, trimmed, sorted, deduped. Copied from fno-agents' `territory`
/// module so this crate stays dependency-free (fno-agents depends on the
/// store, never the reverse); the shared unit tests hold the two equal.
pub fn canonical_scope(scope: &str) -> String {
    let mut members: Vec<String> = scope
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    members.sort();
    members.dedup();
    members.join(",")
}

pub fn parse_rfc3339_ms(ts: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Once a day: `ephemeral` rows older than the schema's TTL floor leave the
/// store. `durable`, `gate`, rejected, and migration receipt rows never
/// auto-expire; stale ingest cursors still fall off on the same cadence.
fn prune(conn: &mut Connection, now_ms: i64) -> Result<(), String> {
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
    let cutoff = now_ms.saturating_sub(MINIMUM_EPHEMERAL_TTL_HOURS * HOUR_MS);
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| e.to_string())?;
    tx.execute(
        "DELETE FROM events WHERE retention_class = 'ephemeral' AND ts_ms < ?1",
        params![cutoff],
    )
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

/// One acknowledged append: the positive readback the caller may trust.
#[derive(Debug, Serialize)]
pub struct AppendReceipt {
    pub store: PathBuf,
    pub event_id: String,
    pub seq: i64,
    pub retention_class: String,
    /// False when the id (or byte-identical line) was already stored: the
    /// retry is an idempotent hit, never a second row.
    pub inserted: bool,
}

/// Commit one canonical envelope as the acknowledgement boundary: a single
/// `BEGIN IMMEDIATE` transaction on a WAL/FULL store, with positive readback
/// by `event_id` before the receipt returns. The envelope text is stored
/// byte-for-byte (single line, no key reordering); a caller that lost its
/// reply re-appends the same envelope and reads back the same row.
///
/// `requested_event_id` is the idempotency key (`append_idempotent`); when
/// `None` one is minted as `evt:<sha256(line)>`, so a byte-identical retry
/// is still an idempotent hit and no duplicate can exist.
pub fn append_envelope(
    journal: &Path,
    envelope_json: &str,
    requested_event_id: Option<&str>,
) -> Result<AppendReceipt, String> {
    let store = store_path(journal);
    let line = envelope_json.trim();
    if line.contains('\n') || line.contains('\r') {
        return Err(format!(
            "{}: envelope must be a single line (refused: embedded newline)",
            store.display()
        ));
    }
    let value: serde_json::Value = serde_json::from_str(line)
        .map_err(|e| format!("{}: envelope is not valid JSON: {e}", store.display()))?;
    let obj = value
        .as_object()
        .ok_or_else(|| format!("{}: envelope is not a JSON object", store.display()))?;
    let ty = obj
        .get("type")
        .and_then(|t| t.as_str())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| format!("{}: envelope has no type", store.display()))?
        .to_string();
    if obj.get("source").and_then(|s| s.as_str()).is_none() {
        return Err(format!("{}: envelope has no source", store.display()));
    }
    if !matches!(obj.get("data"), Some(serde_json::Value::Object(_))) {
        return Err(format!(
            "{}: envelope data is not an object",
            store.display()
        ));
    }
    let ts_str = obj
        .get("ts")
        .and_then(|t| t.as_str())
        .ok_or_else(|| format!("{}: envelope has no ts", store.display()))?;
    let ts_ms = parse_rfc3339_ms(ts_str)
        .ok_or_else(|| format!("{}: envelope ts is not RFC3339: {ts_str}", store.display()))?;
    // A canonical crown scope is validated at append time, the same rule
    // import applies: corruption is refused at the boundary, never stored.
    if let Some(s) = obj
        .get("data")
        .and_then(|d| d.get("scope"))
        .and_then(|s| s.as_str())
    {
        if !is_valid_event_scope(&ty, obj.get("data").expect("data object checked above"), s) {
            return Err(format!(
                "{}: data.scope is not a canonical crown scope: {s}",
                store.display()
            ));
        }
    }

    let event_id = requested_event_id
        .map(str::to_string)
        .unwrap_or_else(|| format!("evt:{}", hex(&Sha256::digest(line.as_bytes()))));
    let row_hash = Sha256::digest(line.as_bytes()).to_vec();
    let class = retention_class(&ty);

    // The commit creates the directory it needs; the caller-side guards
    // (Python's hermetic fence, the shell's opt-in parent guard) already ran.
    if let Some(parent) = store.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", store.display()))?;
    }
    let mut conn = open_store(&store)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| format!("{}: {e}", store.display()))?;
    let id = extract_identity(line);
    let inserted = tx
        .execute(
            "INSERT OR IGNORE INTO events
                 (event_id, row_hash, ts_ms, type, source, scope, retention_class,
                  session_id, node_id, pr_number, head_sha, repo, reject_reason, line)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, NULL, ?13)",
            params![
                event_id,
                row_hash,
                ts_ms,
                ty,
                obj.get("source").and_then(|s| s.as_str()).unwrap_or(""),
                obj.get("data")
                    .and_then(|d| d.get("scope"))
                    .and_then(|s| s.as_str()),
                class,
                id.session_id,
                id.node_id,
                id.pr_number,
                id.head_sha,
                id.repo,
                line,
            ],
        )
        .map_err(|e| format!("{}: {e}", store.display()))?;
    // Positive readback: the row this receipt names must be in the store.
    let (seq, stored_line): (i64, String) = tx
        .query_row(
            "SELECT seq, line FROM events WHERE event_id = ?1",
            params![event_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|e| format!("{}: readback failed after append: {e}", store.display()))?;
    if inserted == 0 && stored_line != line {
        return Err(format!(
            "{}: identity collision on event_id {event_id}: stored envelope differs",
            store.display()
        ));
    }
    tx.commit()
        .map_err(|e| format!("{}: {e}", store.display()))?;
    Ok(AppendReceipt {
        store,
        event_id,
        seq,
        retention_class: class.to_string(),
        inserted: inserted > 0,
    })
}

/// One stored row, in commit order. `line` is the canonical envelope text.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EventRow {
    pub seq: i64,
    pub event_id: String,
    pub ts_ms: i64,
    pub r#type: String,
    pub source: String,
    pub scope: Option<String>,
    pub retention_class: String,
    pub reject_reason: Option<String>,
    pub line: String,
}

/// Typed filters for [`query_events`]; every field is ANDed, absent fields
/// are unfiltered.
#[derive(Debug, Default, Clone)]
pub struct EventQuery {
    pub types: Vec<String>,
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub scope: Option<String>,
    pub session_id: Option<String>,
    pub node_id: Option<String>,
    pub pr_number: Option<i64>,
    pub head_sha: Option<String>,
    pub repo: Option<String>,
    pub source: Option<String>,
    /// Rejected rows (imports that failed validation) are excluded unless
    /// asked for by name; a gate never satisfies itself on one.
    pub include_rejected: bool,
    pub limit: Option<u32>,
}

impl EventQuery {
    /// The row set every journal-text reader parses: `types` plus the empty
    /// type (corrupt and typeless rows, which parsers count), rejected rows
    /// included. No types reads every row.
    pub fn of_types(types: &[&str]) -> Self {
        let mut types: Vec<String> = types.iter().map(|t| t.to_string()).collect();
        if !types.is_empty() {
            types.push(String::new());
        }
        EventQuery {
            types,
            include_rejected: true,
            ..Default::default()
        }
    }

    fn build_sql(&self) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
        let mut where_clauses: Vec<String> = Vec::new();
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        // The IN list is built before the push closure exists, so the two
        // never hold the arg vec at once.
        if !self.types.is_empty() {
            let mut placeholders: Vec<String> = Vec::new();
            for t in &self.types {
                args.push(Box::new(t.clone()));
                placeholders.push("?".to_string());
            }
            where_clauses.push(format!("type IN ({})", placeholders.join(", ")));
        }
        let mut push = |clause: &str, arg: Box<dyn rusqlite::ToSql>| {
            where_clauses.push(clause.to_string());
            args.push(arg);
        };
        if let Some(v) = self.since_ms {
            push("ts_ms >= ?".into(), Box::new(v));
        }
        if let Some(v) = self.until_ms {
            push("ts_ms <= ?".into(), Box::new(v));
        }
        if let Some(v) = self.scope.clone() {
            push("(scope = ? OR scope IS NULL)".into(), Box::new(v));
        }
        if let Some(v) = self.session_id.clone() {
            push("session_id = ?".into(), Box::new(v));
        }
        if let Some(v) = self.node_id.clone() {
            push("node_id = ?".into(), Box::new(v));
        }
        if let Some(v) = self.pr_number {
            push("pr_number = ?".into(), Box::new(v));
        }
        if let Some(v) = self.head_sha.clone() {
            push("head_sha = ?".into(), Box::new(v));
        }
        if let Some(v) = self.repo.clone() {
            push("repo = ?".into(), Box::new(v));
        }
        if let Some(v) = self.source.clone() {
            push("source = ?".into(), Box::new(v));
        }
        if !self.include_rejected {
            where_clauses.push("reject_reason IS NULL".into());
        }
        let where_sql = if where_clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", where_clauses.join(" AND "))
        };
        let limit_sql = self
            .limit
            .map(|n| format!(" LIMIT {n}"))
            .unwrap_or_default();
        (format!("SELECT seq, event_id, ts_ms, type, source, scope, retention_class, reject_reason, line FROM events{where_sql} ORDER BY seq{limit_sql}"), args)
    }
}

/// Query committed rows in commit order. A failure names the store; there is
/// no file-scan fallback.
pub fn query_events(journal: &Path, q: &EventQuery) -> Result<Vec<EventRow>, String> {
    let conn = open_read(&store_path(journal))?;
    let (sql, args) = q.build_sql();
    let mut stmt = conn.prepare(&sql).map_err(|e| format!("{}: {e}", sql))?;
    let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let rows = stmt
        .query_map(refs.as_slice(), |r| {
            Ok(EventRow {
                seq: r.get(0)?,
                event_id: r.get(1)?,
                ts_ms: r.get(2)?,
                r#type: r.get(3)?,
                source: r.get(4)?,
                scope: r.get(5)?,
                retention_class: r.get(6)?,
                reject_reason: r.get(7)?,
                line: r.get(8)?,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

/// Every review-evidence row type a loopcheck parser reads from journal text.
/// One list, so a call site picks a source, never a vocabulary.
#[allow(dead_code)] // used by fno-agents; the fno copy stays byte-identical
pub(crate) const REVIEW_EVENT_TYPES: &[&str] = &[
    "review_attestation",
    "review_coverage",
    "review_finding",
    "review_finding_resolved",
    "review_invocation",
];

/// [`journal_text`] for the review-evidence rows every review reader parses.
#[allow(dead_code)] // used by fno-agents; the fno copy stays byte-identical
pub(crate) fn review_text(journal: &Path) -> String {
    journal_text(journal, REVIEW_EVENT_TYPES)
}

/// The journal text for `types`, complete across rotation generations, in
/// commit order.
///
/// Every writer commits to the store, so the committed rows are the order of
/// record and the live file is at most a raw writer's recent tail. The text
/// is the committed rows the type filter matches (oldest first) followed by
/// the live lines the store does not hold yet. The read never syncs, so a
/// reader never writes. Any store failure reads the live file alone, which
/// never tightens a gate.
pub fn journal_text(journal: &Path, types: &[&str]) -> String {
    journal_text_checked(journal, &EventQuery::of_types(types))
        .unwrap_or_else(|_| std::fs::read_to_string(live_journal(journal)).unwrap_or_default())
}

/// [`journal_text`] for a full [`EventQuery`], with a failed read reported as
/// `Err` instead of the live-file fallback. A journal with neither a live
/// file nor a store reads as empty and creates nothing; a live file or store
/// that exists and cannot be read is `Err` naming it.
pub fn journal_text_checked(journal: &Path, q: &EventQuery) -> Result<String, String> {
    let live = live_journal(journal);
    let live_text = match std::fs::read_to_string(&live) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(format!("{}: {err}", live.display())),
    };
    let store = store_path(&live);
    if !store.is_file() {
        return Ok(live_text);
    }
    let conn = open_read(&store)?;
    let (sql, args) = q.build_sql();
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("{}: {e}", store.display()))?;
    let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let rows = stmt
        .query_map(refs.as_slice(), |r| r.get::<_, String>(8))
        .map_err(|e| format!("{}: {e}", store.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("{}: {e}", store.display()))?;
    let mut held = conn
        .prepare("SELECT 1 FROM events WHERE row_hash = ?1")
        .map_err(|e| format!("{}: {e}", store.display()))?;
    let mut text = String::new();
    for line in rows {
        text.push_str(&line);
        text.push('\n');
    }
    // ponytail: a pre-store line no import took reads as newest; any import fixes it.
    for raw in live_text.lines() {
        if raw.is_empty() {
            continue;
        }
        let hash = Sha256::digest(raw.as_bytes()).to_vec();
        if held
            .query_row(params![hash], |r| r.get::<_, i64>(0))
            .is_err()
        {
            text.push_str(raw);
            text.push('\n');
        }
    }
    Ok(text)
}

/// Write every committed row, in commit order, to `out` as JSONL - atomically
/// (tmp file + rename), labeled by the caller as the snapshot it is. Returns
/// the row count. The store stays authoritative: a failure anywhere removes
/// the temporary and names the output path.
pub fn export_jsonl(journal: &Path, out: &Path) -> Result<u64, String> {
    let conn = open_read(&store_path(journal))?;
    let mut stmt = conn
        .prepare("SELECT line FROM events ORDER BY seq")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    let tmp = out.with_extension("jsonl.export-tmp");
    let mut count = 0u64;
    let write = |tmp: &Path| -> Result<(), String> {
        use std::io::Write;
        let mut fh = std::fs::File::create(tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
        for row in rows {
            let line = row.map_err(|e| e.to_string())?;
            writeln!(fh, "{line}").map_err(|e| format!("{}: {e}", tmp.display()))?;
            count += 1;
        }
        fh.sync_all()
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        Ok(())
    };
    write(&tmp)?;
    if let Err(e) = std::fs::rename(&tmp, out) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("{}: {e}", out.display()));
    }
    Ok(count)
}

/// Prune expired `ephemeral` rows immediately (the `gc` verb's primitive),
/// bypassing the daily gate. Returns the deleted count. `durable`, `gate`,
/// rejected, and migration rows never leave.
pub fn prune_ephemeral_now(journal: &Path, now_ms: i64) -> Result<u64, String> {
    let conn = open_store(&store_path(journal))?;
    let cutoff = now_ms.saturating_sub(MINIMUM_EPHEMERAL_TTL_HOURS * HOUR_MS);
    let n = conn
        .execute(
            "DELETE FROM events WHERE retention_class = 'ephemeral' AND ts_ms < ?1",
            params![cutoff],
        )
        .map_err(|e| e.to_string())?;
    Ok(n as u64)
}

#[cfg(test)]
mod tests;
