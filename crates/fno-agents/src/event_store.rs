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
//! Sync reads a whole file into memory, so one sync's memory scales
//! with the file size. Bounded in practice by the rotation threshold; only a
//! long deferred-rotation window (broken store) grows past it, and that window
//! already screams on stderr per emit. Upgrade path: stream from the cursor
//! offset with a BufReader and carry the partial tail line.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
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

/// Event types the schema declares `retention: telemetry`: high-volume
/// readouts whose readers fold only recent rows. They stay in the main
/// store (no sibling journal) and expire after [`TELEMETRY_TTL_HOURS`].
/// Kept equal to `schema.yaml` by the parity test.
pub const TELEMETRY_EVENT_TYPES: &[&str] = &[
    "codex_thread_inside_leg",
    "control_plane_tick",
    "guard_decision",
    "inside_leg_report",
    "store_seat_lock_unlinked",
    "store_socket_unlinked",
];

/// Retention floor for `ephemeral` rows (`schema.yaml`:
/// `retention.minimum_ephemeral_ttl_hours`).
pub const MINIMUM_EPHEMERAL_TTL_HOURS: i64 = 672;

/// Retention horizon for `telemetry` rows (`schema.yaml`:
/// `retention.telemetry_ttl_hours`).
pub const TELEMETRY_TTL_HOURS: i64 = 168;

/// Retention horizon for `guard_decision` allow rows. They only prove a
/// guard still runs, so a day of them is enough; block rows keep the
/// telemetry horizon.
pub const GUARD_ALLOW_TTL_HOURS: i64 = 24;

/// Marks a judged refusal inside `append_envelope`'s error string. The
/// door strips it and answers exit 3, the class Python maps to
/// `ValidationError`; every other error stays a store fault on exit 1.
pub const VALIDATE_PREFIX: &str = "event-judged: ";

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

/// Whether an event kind belongs to the schema-declared telemetry class.
pub fn is_telemetry_event(kind: &str) -> bool {
    TELEMETRY_EVENT_TYPES.contains(&kind)
}

/// The permanent old->new spellings of the role-event rename. Stored rows
/// are never rewritten (events.db, rotated archives and exports are
/// append-only history), so queries expand a kind to its old spelling and
/// readers canonicalize the row type through [`event_type_alias`]. This
/// table never shrinks.
pub const EVENT_TYPE_ALIASES: &[(&str, &str)] = &[("agent_role_vacated", "agent_team_vacated")];

/// The canonical (new) spelling of an event kind: an old stored spelling
/// maps to its replacement, anything else is itself.
pub fn event_type_alias(kind: &str) -> &str {
    EVENT_TYPE_ALIASES
        .iter()
        .find(|(old, _)| *old == kind)
        .map(|(_, new)| *new)
        .unwrap_or(kind)
}

/// Every stored spelling a query for `types` must match: the asked kinds,
/// their canonical forms, and each one's old aliases.
fn query_types_with_aliases(types: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(types.len() * 2);
    for asked in types {
        let canonical = event_type_alias(asked);
        for candidate in [asked.as_str(), canonical] {
            if !out.iter().any(|t| t == candidate) {
                out.push(candidate.to_string());
            }
        }
        for (old, new) in EVENT_TYPE_ALIASES {
            if *new == canonical && !out.iter().any(|t| t == *old) {
                out.push((*old).to_string());
            }
        }
    }
    out
}

/// The retention class a kind belongs to: `ephemeral`, `gate`, `telemetry`,
/// or `durable` (the schema default).
pub fn retention_class(kind: &str) -> &'static str {
    if is_ephemeral_event(kind) {
        "ephemeral"
    } else if is_gate_event(kind) {
        "gate"
    } else if is_telemetry_event(kind) {
        "telemetry"
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
    /// Lines the observation gate counted as pending instead of storing:
    /// declared poll types whose identical occurrence was already
    /// represented.
    pub coalesced: u64,
    pub read_bytes: u64,
}

#[derive(Default)]
struct FileTally {
    ingested: u64,
    corrupt: u64,
    coalesced: u64,
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
/// A state-root journal (`events`, `decisions`, `questions`) resolves through
/// the layout table instead, so a migrated root answers the `db/` store.
pub fn store_path(journal: &Path) -> PathBuf {
    let live = live_journal(journal);
    let stem = live
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem = stem.strip_suffix(".jsonl").unwrap_or(&stem);
    if let Some(routed) = route_state_root_store(&live, stem) {
        return routed;
    }
    live.with_file_name(format!("{stem}.db"))
}

/// The canonical state root, cached per env fingerprint: an emit pays one
/// derivation per process, and a test that re-pins its home still routes to
/// its own root.
fn route_state_root_store(live: &Path, stem: &str) -> Option<PathBuf> {
    if !matches!(stem, "events" | "decisions" | "questions") {
        return None;
    }
    type Cached = (
        Option<std::ffi::OsString>,
        Option<std::ffi::OsString>,
        PathBuf,
    );
    static ROUTE_ROOT: std::sync::OnceLock<Cached> = std::sync::OnceLock::new();
    let home_env = std::env::var_os("FNO_AGENTS_HOME").filter(|v| !v.is_empty());
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty());
    let root = match ROUTE_ROOT.get() {
        Some((e, h, r)) if *e == home_env && *h == home => r.clone(),
        _ => {
            let mut derived = home
                .as_ref()
                .map(|h| PathBuf::from(h).join(".fno"))
                .unwrap_or_else(|| PathBuf::from(".fno"));
            if let Some(v) = &home_env {
                let pinned = PathBuf::from(v);
                derived = pinned.parent().map(|p| p.to_path_buf()).unwrap_or(pinned);
            }
            let derived = std::fs::canonicalize(&derived).unwrap_or(derived);
            let _ = ROUTE_ROOT.set((home_env, home, derived.clone()));
            derived
        }
    };
    let parent = live.parent()?;
    let parent = std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
    if parent != root {
        return None;
    }
    Some(crate::state_layout::place(&root, &format!("{stem}.db")))
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
    let result = import_sources(&store, sources);
    match result {
        Ok(receipt) => Ok(receipt),
        Err(error) if corrupt_image(&error) => Err(refuse_corrupt_store(&store, &error)),
        Err(error) => Err(error),
    }
}

fn import_sources(store: &Path, sources: &[&Path]) -> Result<SyncReceipt, String> {
    if let Some(parent) = store.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", store.display()))?;
    }
    let mut conn = open_store(store)?;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| format!("{}: {e}", store.display()))?;
    let mut total = FileTally::default();
    for source in sources {
        let tally = import_file(&tx, source, store, now_ms)?;
        total.ingested += tally.ingested;
        total.corrupt += tally.corrupt;
        total.coalesced += tally.coalesced;
        total.read_bytes += tally.read_bytes;
    }
    observation::sweep_expired_windows(&tx, now_ms)
        .map_err(|e| format!("{}: {e}", store.display()))?;
    tx.commit()
        .map_err(|e| format!("{}: {e}", store.display()))?;
    prune(&mut conn, now_ms).map_err(|e| format!("{}: {e}", store.display()))?;
    Ok(SyncReceipt {
        store: store.to_path_buf(),
        ingested: total.ingested,
        corrupt: total.corrupt,
        coalesced: total.coalesced,
        read_bytes: total.read_bytes,
    })
}

/// Open (creating if needed) the store through the store seam, with its
/// schema ensured (v2 created, or v1 migrated) before the connection is
/// handed out.
fn open_store(store: &Path) -> Result<Connection, String> {
    crate::live_store_fence::refuse_worktree_build_on_operator_store(store)?;
    // No integrity sweep here. quick_check walks every page, so per-open it is
    // O(store size), and this opener sits on the per-fire hook append path: a
    // 673 MB store scanned 4-5 times per Bash call drove the fleet load storm.
    // SQLite surfaces page damage as statement errors instead, and
    // `refuse_corrupt_store` turns those into the same loud refusal.
    let mut conn = crate::store_conn::open_write(store)?;
    ensure_schema(&mut conn, store)?;
    observation::ensure_observation_tables(&conn)
        .map_err(|e| format!("{}: {e}", store.display()))?;
    Ok(conn)
}

/// The SQLite diagnostics that mean stored pages are unreadable. Distinct
/// from busy/locked: a corrupt image must never be written again, so the
/// caller refuses instead of retrying.
fn corrupt_image(error: &str) -> bool {
    error.contains("database disk image is malformed")
        || error.contains("file is not a database")
        || error.contains("malformed database schema")
}

/// File the operator attention row for a corrupt store and decorate the
/// error with the refusal. The question id hashes the store identity, so
/// every door that hits the damage dedupes into one attention item.
fn refuse_corrupt_store(store: &Path, error: &str) -> String {
    let parent = store.parent().unwrap_or_else(|| Path::new("."));
    let root = if parent.file_name().is_some_and(|name| name == "db") {
        parent.parent().unwrap_or(parent)
    } else {
        parent
    };
    let attention = root.join("questions.jsonl");
    let mut identity = Sha256::new();
    identity.update(store.as_os_str().as_encoded_bytes());
    if let Ok(meta) = store.metadata() {
        identity.update(meta.dev().to_le_bytes());
        identity.update(meta.ino().to_le_bytes());
    }
    let id = format!("q-store-{:x}", identity.finalize());
    let row = serde_json::json!({"ts": chrono::Utc::now().to_rfc3339(), "type": "operator_question", "source": "rust", "data": {"question_id": id, "question": error, "ask": "Recover an offline copy of the event store; pause writers before any separately approved installation.", "asker": "event-store", "node": "none", "context": {"blocked_because": error, "unknowns": "The original corruption interleaving and live installation safety have not been verified."}, "subject": "event-store-integrity", "blocks": []}});
    use std::io::Write;
    let notice = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&attention)
        .and_then(|mut file| writeln!(file, "{row}"));
    match notice {
        Ok(()) => format!("{error}; write refused; attention: {}", attention.display()),
        Err(e) => format!(
            "{error}; write refused; attention {} failed: {e}",
            attention.display()
        ),
    }
}

/// Read-only handle for history readers; a failure names the store path.
pub(crate) fn upgrade_role_store(store: &Path) -> Result<(), String> {
    open_store(store).map(|_| ()).map_err(|error| {
        if corrupt_image(&error) {
            refuse_corrupt_store(store, &error)
        } else {
            error
        }
    })
}

pub fn open_read(store: &Path) -> Result<Connection, String> {
    let conn = crate::store_conn::open_read(store)?;
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
        caused_by TEXT, \
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

/// The `events_meta` key that stamps the first moment this build observed the
/// store: history proven to be complete starts there, never earlier.
pub const COVERAGE_EPOCH_KEY: &str = "coverage_complete_since_ms";

/// Ensure the store speaks schema v2: a fresh store is created in the v2
/// shape; a v1 store (`events` without an `event_id` column) migrates in
/// place; a v2 store is a no-op. Any failure rolls the transaction back, so
/// `user_version`, row counts, and completion metadata are untouched. Every
/// open also stamps the coverage epoch once (a single indexed select on the
/// common path).
pub fn ensure_schema(conn: &mut Connection, store: &Path) -> Result<(), String> {
    let current = refuse_newer_schema(conn, store)?;
    let already_v2: bool = current >= SCHEMA_VERSION && events_table_has_event_id(conn);
    if already_v2 {
        migrate_caused_by(conn)?;
        crate::role_migration::upgrade_event_store(conn)?;
        return stamp_coverage_epoch(conn, store);
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
        .map_err(|e| format!("{}: migration: {e}", store.display()))?;
    migrate_caused_by(conn)?;
    crate::role_migration::upgrade_event_store(conn)?;
    stamp_coverage_epoch(conn, store)
}

/// The caused_by column on a store created before it existed. The ALTER is
/// PRAGMA-guarded and idempotent; two first opens can race it and the loser
/// tolerates the winner's duplicate-column answer.
fn migrate_caused_by(conn: &Connection) -> Result<(), String> {
    let has: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('events') WHERE name = 'caused_by'",
            [],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    if has > 0 {
        return Ok(());
    }
    if let Err(e) = conn.execute_batch("ALTER TABLE events ADD COLUMN caused_by TEXT") {
        if !e.to_string().contains("duplicate column name") {
            return Err(e.to_string());
        }
    }
    Ok(())
}

/// Stamp [`COVERAGE_EPOCH_KEY`] with the first-open moment of this build,
/// once per store. The value is never taken from `MIN(ts_ms)` or an imported
/// row: imported rows widen what is observed, never what is proven.
fn stamp_coverage_epoch(conn: &Connection, store: &Path) -> Result<(), String> {
    let present: Option<String> = conn
        .query_row(
            "SELECT value FROM events_meta WHERE key = ?1",
            params![COVERAGE_EPOCH_KEY],
            |r| r.get(0),
        )
        .ok();
    if present.is_some() {
        return Ok(());
    }
    let now_ms = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => return Err(format!("{}: {e}", store.display())),
    };
    conn.execute(
        "INSERT OR IGNORE INTO events_meta (key, value) VALUES (?1, ?2)",
        params![COVERAGE_EPOCH_KEY, now_ms.to_string()],
    )
    .map(|_| ())
    .map_err(|e| format!("{}: coverage epoch: {e}", store.display()))
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
              session_id, node_id, pr_number, head_sha, repo, caused_by,
              reject_reason, line)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)"
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
            id.caused_by,
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
    /// The causing event's id, when the envelope names one
    /// (`data.caused_by`).
    pub caused_by: Option<String>,
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
    id.caused_by = get_str(&["caused_by"]);
    id
}

/// Import one journal file's complete lines into the store. Any resolved
/// source works here - rotated, live, ephemeral sibling, an agents lifecycle
/// journal, or shell-writer fragments - because the `row_hash` key dedupes
/// overlap and the class comes from the event type, never the file.
fn import_file(
    tx: &Transaction,
    path: &Path,
    store: &Path,
    now_ms: i64,
) -> Result<FileTally, String> {
    let sql_error = |error: &dyn std::fmt::Display| {
        format!("{}: importing {}: {error}", store.display(), path.display())
    };
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(FileTally::default()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let (dev, ino, len) = (meta.dev() as i64, meta.ino() as i64, meta.len());
    let resume: Option<(Vec<u8>, i64)> = tx
        .query_row(
            "SELECT head_hash, \"offset\" FROM ingest_cursor WHERE dev = ?1 AND ino = ?2",
            params![dev, ino],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(|e| sql_error(&e))?;
    // A frozen journal is read on every import. When the cursor already sits
    // at EOF under the same head line, a bounded head read proves there is
    // nothing new; only the cursor's age is refreshed so prune keeps it.
    if let Some((head, offset)) = &resume {
        if u64::try_from(*offset).is_ok_and(|o| o == len)
            && read_head_hash(path).as_ref() == Some(head)
        {
            tx.execute(
                "UPDATE ingest_cursor SET updated_ms = ?3 WHERE dev = ?1 AND ino = ?2",
                params![dev, ino, now_ms],
            )
            .map_err(|e| sql_error(&e))?;
            return Ok(FileTally::default());
        }
    }
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    // Offsets are BYTE offsets into the raw file, never into a lossy string:
    // a conversion that resizes bytes would desync the stored cursor.
    let head_hash: Vec<u8> = {
        let first = bytes.split(|&b| b == b'\n').next().unwrap_or(&[]);
        Sha256::digest(first).to_vec()
    };
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
        // A declared poll observation consults the gate before inserting:
        // an identical healthy poll inside its window is counted pending and
        // no row is written. Rejected lines never coalesce; they stay
        // verbatim.
        if reject.is_none()
            && observation::OBSERVATION_HEARTBEATS
                .iter()
                .any(|(t, _)| *t == ty.as_str())
        {
            let data = serde_json::from_str::<serde_json::Value>(&line)
                .ok()
                .and_then(|v| v.get("data").cloned())
                .unwrap_or(serde_json::Value::Null);
            match observation::observation_gate(
                &tx,
                &ty,
                scope.as_deref(),
                &data,
                &line,
                &row_hash,
                ts_ms,
            ) {
                Ok(observation::ObservationGate::Suppressed { .. }) => {
                    tally.coalesced += 1;
                    continue;
                }
                Ok(observation::ObservationGate::Insert) => {}
                Err(e) => return Err(sql_error(&e)),
            }
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
        .map_err(|e| sql_error(&e))?;
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
    .map_err(|e| sql_error(&e))?;
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
            return reject(ts_ms, ty, source, "scope is not a canonical team scope");
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

/// A canonical team scope is comma-joined sorted members, none blank, none
/// carrying whitespace (that is how the rendered-board corruption of
/// 2026-09-14 is detectable at write time).
fn is_canonical_team_scope(s: &str) -> bool {
    !s.is_empty()
        && canonical_scope(s) == s
        && s.split(',')
            .all(|m| !m.is_empty() && !m.chars().any(char::is_whitespace))
}

fn is_valid_event_scope(event_type: &str, _data: &serde_json::Value, scope: &str) -> bool {
    if !scope.is_empty() {
        return is_canonical_team_scope(scope);
    }
    // A stop_decision with no team scope remains an auditable event: the
    // correlated session row is what lead admission reads, and a fresh successor
    // journals exactly there - before init writes the manifest that would
    // carry its scope.
    event_type == "stop_decision"
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
/// store, and stale ingest cursors fall off. `telemetry` rows expire on their
/// own gate ([`prune_telemetry_due`]). `durable`, `gate`, rejected, and
/// migration receipt rows never auto-expire.
fn prune(conn: &mut Connection, now_ms: i64) -> Result<(), String> {
    if now_ms.saturating_sub(meta_ms(conn, "last_prune_ms")) >= DAY_MS {
        let cutoff = now_ms.saturating_sub(MINIMUM_EPHEMERAL_TTL_HOURS * HOUR_MS);
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        // Re-read under the write lock: a concurrent sync that already claimed
        // this pass wins, so two processes never prune at once.
        if now_ms.saturating_sub(meta_ms(&tx, "last_prune_ms")) >= DAY_MS {
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
            stamp_meta(&tx, "last_prune_ms", now_ms)?;
            tx.commit().map_err(|e| e.to_string())?;
        }
    }
    // Best-effort: the ingest already committed, so a busy store here never
    // fails the sync. A pass that errors keeps its backlog mark and retries.
    let _ = prune_telemetry_due(conn, now_ms);
    Ok(())
}

/// An integer `events_meta` value, 0 when absent or unreadable.
fn meta_ms(conn: &Connection, key: &str) -> i64 {
    conn.query_row(
        "SELECT value FROM events_meta WHERE key = ?1",
        params![key],
        |r| r.get::<_, String>(0),
    )
    .ok()
    .and_then(|s| s.parse().ok())
    .unwrap_or(0)
}

fn stamp_meta(conn: &Connection, key: &str, value: i64) -> Result<(), String> {
    conn.execute(
        "INSERT INTO events_meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = ?2",
        params![key, value.to_string()],
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

/// The true start of the last telemetry pass. Coverage reads its cutoff from
/// here, so it never claims a range a pass already deleted from.
const TELEMETRY_PRUNED_KEY: &str = "telemetry_pruned_ms";

/// 1 while a telemetry pass has rows left (it spent its budget or failed).
const TELEMETRY_BACKLOG_KEY: &str = "telemetry_backlog";

/// Rows per telemetry delete. Each batch commits on its own and stays well
/// inside the 5s busy wait an append rides, so the first pass over a large
/// backlog never starves a live writer.
const TELEMETRY_PRUNE_BATCH: i64 = 1_000;

/// Wall-clock cap on one pass's telemetry deletes. The sync caller pays it,
/// so a large backlog costs any one command a few seconds at most.
const TELEMETRY_PRUNE_BUDGET: std::time::Duration = std::time::Duration::from_secs(3);

/// How soon a pass that left a backlog runs again.
const PRUNE_RETRY_MS: i64 = 5 * 60_000;

fn telemetry_due(conn: &Connection, now_ms: i64) -> bool {
    let wait = if meta_ms(conn, TELEMETRY_BACKLOG_KEY) != 0 {
        PRUNE_RETRY_MS
    } else {
        DAY_MS
    };
    now_ms.saturating_sub(meta_ms(conn, TELEMETRY_PRUNED_KEY)) >= wait
}

/// Run a telemetry pass when one is due: daily, or every
/// [`PRUNE_RETRY_MS`] while a backlog is left. The pass is claimed under
/// the write lock with the backlog mark already set, so a pass that dies
/// midway is retried rather than forgotten for a day.
fn prune_telemetry_due(conn: &mut Connection, now_ms: i64) -> Result<(), String> {
    if !telemetry_due(conn, now_ms) {
        return Ok(());
    }
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| e.to_string())?;
    if !telemetry_due(&tx, now_ms) {
        return Ok(());
    }
    stamp_meta(&tx, TELEMETRY_PRUNED_KEY, now_ms)?;
    stamp_meta(&tx, TELEMETRY_BACKLOG_KEY, 1)?;
    tx.commit().map_err(|e| e.to_string())?;
    if prune_telemetry(conn, now_ms, TELEMETRY_PRUNE_BUDGET)? {
        stamp_meta(conn, TELEMETRY_BACKLOG_KEY, 0)?;
    }
    Ok(())
}

/// Delete telemetry rows past their horizon, oldest first within each
/// kind, until none are left (`true`) or `budget` is spent (`false`). The
/// match is by kind, not class: rows stored before their kind joined the
/// class still read `durable`. Rejected rows stay. `guard_decision` allow
/// rows then go on their shorter [`GUARD_ALLOW_TTL_HOURS`] horizon.
fn prune_telemetry(
    conn: &Connection,
    now_ms: i64,
    budget: std::time::Duration,
) -> Result<bool, String> {
    let started = std::time::Instant::now();
    let cutoff = now_ms.saturating_sub(TELEMETRY_TTL_HOURS * HOUR_MS);
    let allow_cutoff = now_ms.saturating_sub(GUARD_ALLOW_TTL_HOURS * HOUR_MS);
    let passes = TELEMETRY_EVENT_TYPES
        .iter()
        .map(|kind| (*kind, cutoff, ""))
        .chain(std::iter::once((
            "guard_decision",
            allow_cutoff,
            " AND json_extract(line, '$.data.decision') = 'allow'",
        )));
    for (kind, cutoff_ms, filter) in passes {
        let sql = format!(
            "DELETE FROM events WHERE seq IN (SELECT seq FROM events \
             WHERE type = ?1 AND ts_ms < ?2 AND reject_reason IS NULL{filter} \
             ORDER BY ts_ms LIMIT ?3)"
        );
        loop {
            if started.elapsed() >= budget {
                return Ok(false);
            }
            let deleted = conn
                .execute(&sql, params![kind, cutoff_ms, TELEMETRY_PRUNE_BATCH])
                .map_err(|e| e.to_string())?;
            if (deleted as i64) < TELEMETRY_PRUNE_BATCH {
                break;
            }
        }
    }
    Ok(true)
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
    /// True when the observation gate represented this poll inside its
    /// subject's window instead of storing a row: nothing was inserted, and
    /// `pending_occurrences` names the window's pending count.
    pub suppressed: bool,
    pub pending_occurrences: i64,
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
    // The judge owns the schema now: one line, the same diagnostic the
    // Python judge printed. Storage-level checks (ts keying, scope
    // canonicality) run after it and keep their own wording. The prefix
    // marks the refusal so the door answers exit 3: a judged refusal is a
    // different failure class than a store fault, and Python raises
    // ValidationError only for the former.
    if let Err(msg) = validate::validate_envelope(obj) {
        return Err(format!("{VALIDATE_PREFIX}{msg}"));
    }
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
    // A canonical team scope is validated at append time, the same rule
    // import applies: corruption is refused at the boundary, never stored.
    if let Some(s) = obj
        .get("data")
        .and_then(|d| d.get("scope"))
        .and_then(|s| s.as_str())
    {
        if !is_valid_event_scope(&ty, obj.get("data").expect("data object checked above"), s) {
            return Err(format!(
                "{}: data.scope is not a canonical team scope: {s}",
                store.display()
            ));
        }
    }

    let event_id = requested_event_id
        .map(str::to_string)
        .unwrap_or_else(|| format!("evt:{}", hex(&Sha256::digest(line.as_bytes()))));
    let row_hash = Sha256::digest(line.as_bytes()).to_vec();
    let class = retention_class(&ty);

    // Python's door client already retries the same policy
    // (store_client.py:213): the store's 5s busy wait expires under
    // fork-heavy contention, and one expired wait must not lose the row.
    // The event id is the sha256 of the line, so a retried append reads
    // back as an idempotent hit, never a duplicate.
    let mut last_error = String::new();
    for attempt in 0..APPEND_ATTEMPTS {
        match commit_envelope(&store, line, &event_id, &row_hash, &ty, class, obj, ts_ms) {
            Ok(receipt) => return Ok(receipt),
            Err(error) => {
                let settled = !lock_busy(&error) || attempt + 1 == APPEND_ATTEMPTS;
                last_error = error;
                if settled {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(250 * (attempt as u64 + 1)));
            }
        }
    }
    report_lost_row(&store, &ty, &last_error);
    if corrupt_image(&last_error) {
        return Err(refuse_corrupt_store(&store, &last_error));
    }
    Err(last_error)
}

/// How many commit attempts [`append_envelope`] makes before the row is
/// reported lost. Python's store_client carries the same bound.
const APPEND_ATTEMPTS: usize = 3;

/// The diagnostic SQLite answers when the busy wait expired. Python's
/// store_client matches the same string at store_client.py:232.
fn lock_busy(error: &str) -> bool {
    error.contains("database is locked")
}

/// The dead-letter line a finally-lost row leaves behind: every commit
/// attempt failed, the row exists nowhere, and a plain file append is the
/// only trace that does not contend with the store that refused the write.
/// Best-effort by design: a failed sidecar write eprintlns, and the
/// original error still returns to the caller.
fn report_lost_row(store: &Path, type_name: &str, error: &str) {
    use std::io::Write;
    let sidecar = PathBuf::from(format!("{}.lost.jsonl", store.display()));
    let row = serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "type": type_name,
        "store": store.display().to_string(),
        "error": error,
    });
    let write = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&sidecar)
        .and_then(|mut file| writeln!(file, "{row}"));
    if let Err(e) = write {
        eprintln!(
            "event store: lost {type_name:?} row and could not record it in {}: {e}",
            sidecar.display()
        );
    }
}

/// One commit attempt: the exact body `append_envelope` ran before the
/// retry loop existed, from the directory create through the positive
/// readback. Deterministic ids make any attempt after a half-landed
/// predecessor an idempotent hit.
fn commit_envelope(
    store: &Path,
    line: &str,
    event_id: &str,
    row_hash: &[u8],
    ty: &str,
    class: &str,
    obj: &serde_json::Map<String, serde_json::Value>,
    ts_ms: i64,
) -> Result<AppendReceipt, String> {
    // The commit creates the directory it needs; the caller-side guards
    // (Python's hermetic fence, the shell's opt-in parent guard) already ran.
    if let Some(parent) = store.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", store.display()))?;
    }
    let mut conn = open_store(store)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| format!("{}: {e}", store.display()))?;
    let id = extract_identity(line);
    // Idempotent hit first: a byte-identical retry reads back the stored row
    // and never consults the observation gate (one logical write, not a new
    // occurrence).
    let existing: Option<i64> = tx
        .query_row(
            "SELECT seq FROM events WHERE event_id = ?1 AND line = ?2",
            params![event_id, line],
            |r| r.get(0),
        )
        .ok();
    if let Some(seq) = existing {
        tx.commit()
            .map_err(|e| format!("{}: {e}", store.display()))?;
        return Ok(AppendReceipt {
            store: store.to_path_buf(),
            event_id: event_id.to_string(),
            seq,
            retention_class: class.to_string(),
            inserted: false,
            suppressed: false,
            pending_occurrences: 0,
        });
    }
    let collided: Option<i64> = tx
        .query_row(
            "SELECT 1 FROM events WHERE event_id = ?1",
            params![event_id],
            |r| r.get(0),
        )
        .ok();
    if collided.is_some() {
        return Err(format!(
            "{}: identity collision on event_id {event_id}: stored envelope differs",
            store.display()
        ));
    }
    // A declared poll observation consults the gate before inserting: an
    // identical healthy poll inside its window is counted pending and no
    // row is written.
    match observation::observation_gate(
        &tx,
        &ty,
        obj.get("data")
            .and_then(|d| d.get("scope"))
            .and_then(|s| s.as_str()),
        obj.get("data").expect("data object checked above"),
        line,
        &row_hash,
        ts_ms,
    )? {
        observation::ObservationGate::Suppressed { pending } => {
            tx.commit()
                .map_err(|e| format!("{}: {e}", store.display()))?;
            return Ok(AppendReceipt {
                store: store.to_path_buf(),
                event_id: event_id.to_string(),
                seq: 0,
                retention_class: class.to_string(),
                inserted: false,
                suppressed: true,
                pending_occurrences: pending,
            });
        }
        observation::ObservationGate::Insert => {}
    }
    let inserted = tx
        .execute(
            "INSERT OR IGNORE INTO events
                 (event_id, row_hash, ts_ms, type, source, scope, retention_class,
                  session_id, node_id, pr_number, head_sha, repo, caused_by,
                  reject_reason, line)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, NULL, ?14)",
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
                id.caused_by,
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
        store: store.to_path_buf(),
        event_id: event_id.to_string(),
        seq,
        retention_class: class.to_string(),
        inserted: inserted > 0,
        suppressed: false,
        pending_occurrences: 0,
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
    pub history_only: bool,
    pub recovery_batch: Option<String>,
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
    /// Commit-order window `after_seq < seq <= until_seq`: an incremental
    /// reader keeps `after_seq` as its cursor and reads only newer rows.
    pub after_seq: Option<i64>,
    pub until_seq: Option<i64>,
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

    fn build_sql(&self, recovery: bool) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
        let mut where_clauses: Vec<String> = Vec::new();
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        // The IN list is built before the push closure exists, so the two
        // never hold the arg vec at once.
        if !self.types.is_empty() {
            let expanded = query_types_with_aliases(&self.types);
            let mut placeholders: Vec<String> = Vec::new();
            for t in &expanded {
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
        if let Some(v) = self.after_seq {
            push("seq > ?".into(), Box::new(v));
        }
        if let Some(v) = self.until_seq {
            push("seq <= ?".into(), Box::new(v));
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
            .map(|n| format!(" ORDER BY seq LIMIT {n}"))
            .unwrap_or_default();
        let history = if recovery {
            "EXISTS(SELECT 1 FROM recovery_history h WHERE h.event_id = events.event_id)"
        } else {
            "0"
        };
        let batch = if recovery {
            "(SELECT batch FROM recovery_history h WHERE h.event_id = events.event_id)"
        } else {
            "NULL"
        };
        let columns = format!("seq, event_id, ts_ms, type, source, scope, retention_class, reject_reason, line, {history}, {batch}");
        // An indexed filter reads rows out of commit order. The filter then
        // picks seqs in a subquery, so the sort holds integers only. Sorting
        // the selected rows sorted every `line` in a temp B-tree, which
        // spilled gigabytes to disk on each daemon tick. With no indexed
        // filter, a plain scan is already in seq order.
        let indexed = !self.types.is_empty()
            || self.scope.is_some()
            || self.session_id.is_some()
            || self.node_id.is_some()
            || self.pr_number.is_some()
            || self.head_sha.is_some();
        let sql = if indexed {
            format!("SELECT {columns} FROM events WHERE seq IN (SELECT seq FROM events{where_sql}{limit_sql}) ORDER BY seq")
        } else {
            let limit = self
                .limit
                .map(|n| format!(" LIMIT {n}"))
                .unwrap_or_default();
            format!("SELECT {columns} FROM events{where_sql} ORDER BY seq{limit}")
        };
        (sql, args)
    }
}

/// Query committed rows in commit order. A failure names the store; there is
/// no file-scan fallback.
pub fn query_events(journal: &Path, q: &EventQuery) -> Result<Vec<EventRow>, String> {
    let conn = open_read(&store_path(journal))?;
    let (sql, args) = q.build_sql(has_recovery_history(&conn)?);
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
                history_only: r.get(9)?,
                recovery_batch: r.get(10)?,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

/// The newest committed seq, or 0 for an empty store: the high-water mark an
/// incremental reader bounds one pass by and stores as its next cursor.
pub fn max_seq(journal: &Path) -> Result<i64, String> {
    let conn = open_read(&store_path(journal))?;
    conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |r| r.get(0))
        .map_err(|e| e.to_string())
}

fn has_recovery_history(conn: &Connection) -> Result<bool, String> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='recovery_history')",
        [],
        |r| r.get(0),
    )
    .map_err(|e| e.to_string())
}

/// Recovery provenance is read metadata. The canonical envelope stays unchanged.
pub fn recovery_envelope(row: &EventRow) -> serde_json::Value {
    let mut value = serde_json::from_str::<serde_json::Value>(&row.line)
        .unwrap_or_else(|_| serde_json::json!({"_corrupt": row.line}));
    if let Some(object) = value.as_object_mut() {
        object.insert("_store_seq".into(), row.seq.into());
        object.insert("_history_only".into(), row.history_only.into());
        if let Some(batch) = &row.recovery_batch {
            object.insert("_recovery_batch".into(), batch.clone().into());
        }
    }
    value
}

/// The line identity work can append for a journal-text read: the raw stored
/// line when `_store_seq` names this very envelope in THIS store, otherwise
/// the input with its recovery annotations stripped. [`journal_text`]
/// re-serializes committed rows with `_store_seq`/`_history_only` once the
/// store holds recovery history, and identity work (row-hash lookup,
/// idempotent re-append) must never hash - or store - the annotated render.
/// A line read from a sibling store carries a foreign seq, so its verified
/// resolution fails and the stripped render stands in; for a line without
/// annotations this is the verbatim input.
pub fn raw_stored_line(journal: &Path, line: &str) -> Result<String, String> {
    let value = match serde_json::from_str::<serde_json::Value>(line) {
        Ok(value) => value,
        Err(_) => return Ok(line.to_string()),
    };
    let annotated = ["_store_seq", "_history_only", "_recovery_batch"]
        .iter()
        .any(|key| value.get(key).is_some());
    if !annotated {
        return Ok(line.to_string());
    }
    let mut stripped = value.clone();
    if let Some(object) = stripped.as_object_mut() {
        for key in ["_store_seq", "_history_only", "_recovery_batch"] {
            object.remove(key);
        }
    }
    let clean = serde_json::to_string(&stripped).unwrap_or_else(|_| line.to_string());
    let Some(seq) = value.get("_store_seq").and_then(serde_json::Value::as_i64) else {
        return Ok(clean);
    };
    let store = store_path(journal);
    if !store.is_file() {
        return Ok(clean);
    }
    let conn = open_read(&store)?;
    let stored: Option<String> = conn
        .query_row(
            "SELECT line FROM events WHERE seq = ?1",
            params![seq],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| format!("{}: {e}", store.display()))?;
    let Some(stored) = stored else {
        return Ok(clean);
    };
    let same = serde_json::from_str::<serde_json::Value>(&stored)
        .map(|raw| raw == stripped)
        .unwrap_or(false);
    Ok(if same { stored } else { clean })
}

/// Keep recovered history for folds, but never let it restart activity. A
/// recovered ask also suppresses the old answer it newly makes addressable.
pub fn activity_text(text: &str) -> String {
    let rows: Vec<serde_json::Value> = text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let asks: std::collections::HashMap<String, i64> = rows
        .iter()
        .filter(|v| v["type"] == "operator_question" && v["_history_only"] == true)
        .filter_map(|v| {
            Some((
                v["data"]["question_id"].as_str()?.to_owned(),
                v["_store_seq"].as_i64()?,
            ))
        })
        .collect();
    rows.into_iter()
        .filter(|v| {
            if v["type"] == "attention_delivery" {
                return true;
            }
            if v["_history_only"] == true {
                return false;
            }
            let id = v["data"]["question_id"]
                .as_str()
                .or_else(|| v["data"]["item_id"].as_str());
            if matches!(
                v["type"].as_str(),
                Some("attention_answer" | "operator_question_closed")
            ) {
                if let Some(boundary) = id.and_then(|id| asks.get(id)) {
                    return v["_store_seq"].as_i64().unwrap_or(i64::MAX) > *boundary;
                }
            }
            true
        })
        .map(|v| format!("{v}\n"))
        .collect()
}

/// How far back a count over this store is proven, for the asked types. The
/// store keeps every durable and gate row since the coverage epoch and every
/// unexpired ephemeral row; anything before the proven start is unknown, so a
/// zero there is never a confident zero.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Coverage {
    /// `complete` | `partial` | `unknown` | `unreadable`.
    pub status: &'static str,
    /// The proven start (epoch, or the later ephemeral cutoff).
    pub complete_since_ms: Option<i64>,
    pub requested_since_ms: Option<i64>,
    /// Earliest and latest surviving row for the asked types; the observed
    /// span may reach before the epoch (imported history) without proving it.
    pub observed_first_ms: Option<i64>,
    pub observed_last_ms: Option<i64>,
    /// Why the status is not `complete` or `partial`.
    pub reason: Option<String>,
}

/// The coverage receipt for one store and an optional `since` bound. Read
/// only: it never creates or migrates the store, so a pre-epoch store (no
/// stamp yet) reads `unknown`, and a missing or unopenable store reads
/// `unreadable`.
pub fn coverage(journal: &Path, since_ms: Option<i64>, types: &[String]) -> Coverage {
    let store = store_path(journal);
    if !store.is_file() {
        return Coverage {
            status: "unreadable",
            complete_since_ms: None,
            requested_since_ms: since_ms,
            observed_first_ms: None,
            observed_last_ms: None,
            reason: Some(format!("store {} does not exist", store.display())),
        };
    }
    let conn = match open_read(&store) {
        Ok(c) => c,
        Err(e) => {
            return Coverage {
                status: "unreadable",
                complete_since_ms: None,
                requested_since_ms: since_ms,
                observed_first_ms: None,
                observed_last_ms: None,
                reason: Some(e),
            }
        }
    };
    let epoch: Option<i64> = conn
        .query_row(
            "SELECT value FROM events_meta WHERE key = ?1",
            params![COVERAGE_EPOCH_KEY],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|s| s.parse().ok());
    let Some(epoch) = epoch else {
        return Coverage {
            status: "unknown",
            complete_since_ms: None,
            requested_since_ms: since_ms,
            observed_first_ms: None,
            observed_last_ms: None,
            reason: Some("store predates the coverage epoch (no stamp)".to_string()),
        };
    };
    let last_prune_ms: i64 = conn
        .query_row(
            "SELECT value FROM events_meta WHERE key = 'last_prune_ms'",
            [],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    // The proven start per asked kind: durable and gate kinds are kept
    // forever since the epoch; an ephemeral or telemetry kind only proves
    // back to the last prune's cutoff for its class. A mixed list takes the
    // later start.
    let ephemeral_cutoff = last_prune_ms.saturating_sub(MINIMUM_EPHEMERAL_TTL_HOURS * HOUR_MS);
    let telemetry_pruned_ms = meta_ms(&conn, TELEMETRY_PRUNED_KEY);
    let telemetry_cutoff = telemetry_pruned_ms.saturating_sub(TELEMETRY_TTL_HOURS * HOUR_MS);
    let guard_cutoff = telemetry_pruned_ms.saturating_sub(GUARD_ALLOW_TTL_HOURS * HOUR_MS);
    let proven_start = types
        .iter()
        .map(|t| {
            if is_ephemeral_event(t) {
                epoch.max(ephemeral_cutoff)
            } else if t.as_str() == "guard_decision" {
                // Its allow rows only prove back one day.
                epoch.max(guard_cutoff)
            } else if is_telemetry_event(t) {
                epoch.max(telemetry_cutoff)
            } else {
                epoch
            }
        })
        .max()
        .unwrap_or(epoch);
    let (observed_first_ms, observed_last_ms): (Option<i64>, Option<i64>) = {
        let (mut where_clauses, mut args): (Vec<String>, Vec<Box<dyn rusqlite::ToSql>>) =
            (Vec::new(), Vec::new());
        if !types.is_empty() {
            let placeholders = types.iter().map(|_| "?".to_string()).collect::<Vec<_>>();
            where_clauses.push(format!("type IN ({})", placeholders.join(", ")));
            for t in types {
                args.push(Box::new(t.clone()));
            }
        }
        where_clauses.push("reject_reason IS NULL".to_string());
        let sql = format!(
            "SELECT MIN(ts_ms), MAX(ts_ms) FROM events WHERE {}",
            where_clauses.join(" AND ")
        );
        let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
        conn.query_row(&sql, refs.as_slice(), |r| {
            Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<i64>>(1)?))
        })
        .unwrap_or((None, None))
    };
    let complete = since_ms.is_some_and(|s| s >= proven_start);
    Coverage {
        status: if complete { "complete" } else { "partial" },
        complete_since_ms: Some(proven_start),
        requested_since_ms: since_ms,
        observed_first_ms,
        observed_last_ms,
        reason: None,
    }
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
    let store = store_path(&live);
    if !store.is_file() {
        // No store: read the journal that was named. A rotated path must
        // yield its own bytes, not the live file live_journal folds it into
        // - folding double-counted the live lines under the rotation's name
        // and blamed a corrupt live line on the rotation.
        let direct = if journal.is_file() {
            journal
        } else {
            live.as_path()
        };
        return match std::fs::read_to_string(direct) {
            Ok(text) => Ok(text),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(err) => Err(format!("{}: {err}", direct.display())),
        };
    }
    let conn = open_read(&store)?;
    let recovery = has_recovery_history(&conn)?;
    let (sql, args) = q.build_sql(recovery);
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("{}: {e}", store.display()))?;
    let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let rows = stmt
        .query_map(refs.as_slice(), |r| {
            let line: String = r.get(8)?;
            if !recovery {
                return Ok(line);
            }
            let mut value: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => return Ok(line),
            };
            if let Some(object) = value.as_object_mut() {
                object.insert("_store_seq".into(), r.get::<_, i64>(0)?.into());
                object.insert("_history_only".into(), r.get::<_, bool>(9)?.into());
                if let Some(batch) = r.get::<_, Option<String>>(10)? {
                    object.insert("_recovery_batch".into(), batch.into());
                }
            }
            Ok(value.to_string())
        })
        .map_err(|e| format!("{}: {e}", store.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("{}: {e}", store.display()))?;
    let mut held = conn
        .prepare(
            // A line the store holds either as an event or as a pending
            // observation is committed knowledge; only the latter exists for a
            // suppressed poll, and treating it as an uncommitted tail would
            // double-represent it in every journal-text read.
            "SELECT EXISTS(SELECT 1 FROM events WHERE row_hash = ?1)
         OR EXISTS(SELECT 1 FROM event_observation_pending WHERE row_hash = ?1)",
        )
        .map_err(|e| format!("{}: {e}", store.display()))?;
    let mut text = String::new();
    for line in rows {
        text.push_str(&line);
        text.push('\n');
    }
    // The ingest cursor records how far import_file ingested this inode.
    // Every complete line before `offset` is committed knowledge - it sits in
    // events or event_observation_pending, the two tables `held` probes - so
    // the read pulls the tail past the cursor alone: head hash from a bounded
    // 256 KB head read, tail bytes by seek, never the whole journal (the
    // whole-file slurp cost 118 MB of reads per read on the live store). A
    // stale cursor (the head line changed, the file shrank) or an absent one
    // falls back to the full-file scan, the pre-cursor behavior.
    let start: u64 = match std::fs::metadata(&live) {
        Ok(meta) => {
            let cursor = conn
                .query_row(
                    "SELECT head_hash, \"offset\" FROM ingest_cursor WHERE dev = ?1 AND ino = ?2",
                    params![meta.dev(), meta.ino()],
                    |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?)),
                )
                .ok();
            match (read_head_hash(&live), cursor) {
                (Some(hash), Some((head, offset)))
                    if head == hash && offset >= 0 && offset as u64 <= meta.len() =>
                {
                    offset as u64
                }
                _ => 0,
            }
        }
        Err(_) => 0,
    };
    let tail = match read_range(&live, start) {
        Ok(tail) => tail,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(err) => return Err(format!("{}: {err}", live.display())),
    };
    let complete_end = tail.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    // ponytail: a pre-store line no import took reads as newest; any import fixes it.
    for line_bytes in tail[..complete_end].split(|&b| b == b'\n') {
        let line_bytes = line_bytes.strip_suffix(b"\r").unwrap_or(line_bytes);
        if line_bytes.is_empty() {
            continue;
        }
        let hash = Sha256::digest(line_bytes).to_vec();
        let not_held: bool = held
            .query_row(params![hash], |r| r.get::<_, i64>(0))
            .map(|found| found == 0)
            .unwrap_or(false);
        if not_held {
            let raw = String::from_utf8_lossy(line_bytes);
            if observation::tail_line_flushed(&conn, &raw) {
                // The line is represented by a flushed observation window; a
                // second copy in the tail would double-represent it.
                continue;
            }
            text.push_str(&raw);
            text.push('\n');
        }
    }
    Ok(text)
}

/// sha256 of the journal's first line, from a bounded 256 KB head read; a
/// longer first line or an unreadable file reads None, and the caller takes
/// the full-scan fallback. Mirrors import_file's head-hash contract.
fn read_head_hash(live: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut fh = std::fs::File::open(live).ok()?;
    let mut buf = vec![0u8; 262_144];
    let n = fh.read(&mut buf).ok()?;
    let first = buf[..n].split(|&b| b == b'\n').next().unwrap_or(&[]);
    Some(Sha256::digest(first).to_vec())
}

/// Read the file's bytes from `start` to EOF. start beyond EOF reads empty.
fn read_range(path: &Path, start: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut fh = std::fs::File::open(path)?;
    fh.seek(SeekFrom::Start(start))?;
    let mut out = Vec::new();
    fh.read_to_end(&mut out)?;
    Ok(out)
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

/// What one gc pass over the store saw: every row, the rejected ones, and the
/// expired `ephemeral` rows it deleted (or, on a dry run, would delete).
#[derive(Debug, Default, Serialize)]
pub struct GcReceipt {
    pub scanned: i64,
    pub malformed: i64,
    pub expired: i64,
}

/// The `gc` verb's primitive: delete `ephemeral` rows older than `cutoff_ms`,
/// bypassing the daily gate. `durable`, `gate`, rejected and migration rows
/// never leave. A journal with no store reads as an empty receipt, and the
/// store is not created.
pub fn gc_ephemeral(journal: &Path, cutoff_ms: i64, dry_run: bool) -> Result<GcReceipt, String> {
    let store = store_path(journal);
    let result = gc_ephemeral_inner(&store, cutoff_ms, dry_run);
    match result {
        Ok(receipt) => Ok(receipt),
        Err(error) if corrupt_image(&error) => Err(refuse_corrupt_store(&store, &error)),
        Err(error) => Err(error),
    }
}

fn gc_ephemeral_inner(store: &Path, cutoff_ms: i64, dry_run: bool) -> Result<GcReceipt, String> {
    if !store.exists() {
        return Ok(GcReceipt::default());
    }
    let conn = open_store(store)?;
    let named = |e: rusqlite::Error| format!("{}: {e}", store.display());
    let (scanned, malformed) = conn
        .query_row(
            "SELECT count(*), coalesce(sum(reject_reason IS NOT NULL), 0) FROM events",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(named)?;
    let expired = if dry_run {
        conn.query_row(
            "SELECT count(*) FROM events WHERE retention_class = 'ephemeral' AND ts_ms < ?1",
            params![cutoff_ms],
            |r| r.get(0),
        )
        .map_err(named)?
    } else {
        conn.execute(
            "DELETE FROM events WHERE retention_class = 'ephemeral' AND ts_ms < ?1",
            params![cutoff_ms],
        )
        .map_err(named)? as i64
    };
    Ok(GcReceipt {
        scanned,
        malformed,
        expired,
    })
}

mod observation;

pub mod validate;

#[cfg(test)]
mod tests;
