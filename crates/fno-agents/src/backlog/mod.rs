//! The backlog store: graph.db lifecycle (schema, import, sync, export)
//! and the backend naming. SQLite is the only store; `open` creates the db
//! on first use and imports a seed graph.json fixture when present. Each
//! aggregate's tables live in its owning module and no other file writes
//! them (ruling 4); `TABLE_OWNERS` names the owners and the
//! table_ownership test enforces it. Every mutation writes only the
//! changed nodes' rows in one transaction.

pub mod api;
pub mod commands;
pub mod comments;
pub mod decisions;
pub mod encounters;
pub mod model;
pub mod node_state;
pub mod nodes;
pub mod note_cli;
pub mod note_history;
pub mod note_migrate;
pub mod note_stale;
pub mod patch;
pub mod pull_requests;
pub mod receipt;
pub mod relations;
pub mod search;
pub mod sessions;

use crate::backlog::model::Node;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The schema stamp the import writes after the blob `entries` table is
/// dropped.
pub const SCHEMA_VERSION: &str = "3";

/// Each aggregate's owning module (ruling 4). The table_ownership test
/// scans src/ against this map: a write to an owned table outside its
/// owner's file fails naming file and line.
pub const TABLE_OWNERS: &[(&str, &str)] = &[
    ("nodes", "backlog/nodes.rs"),
    ("node_claims", "backlog/nodes.rs"),
    ("node_dispatch", "backlog/nodes.rs"),
    ("node_provenance", "backlog/nodes.rs"),
    ("supersessions", "backlog/nodes.rs"),
    ("relations", "backlog/relations.rs"),
    ("nodes_fts", "backlog/search.rs"),
    ("comments", "backlog/comments.rs"),
    ("encounters", "backlog/encounters.rs"),
    ("pull_requests", "backlog/pull_requests.rs"),
    ("sessions", "backlog/sessions.rs"),
    ("decisions", "backlog/decisions.rs"),
    ("node_decisions", "backlog/decisions.rs"),
];

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis())
        .unwrap_or(0)
}

pub fn content_version(entries: &[Value]) -> String {
    use sha2::{Digest, Sha256};

    let mut hash = Sha256::new();
    for row in entries {
        hash.update(crate::graph_store::to_python_json(row).as_bytes());
        hash.update([0]);
    }
    format!("sqlite:{:x}", hash.finalize())
}

pub fn database_path(graph: &Path) -> PathBuf {
    graph.with_extension("db")
}

/// The backend the store names in `graph_meta.backend`. Unset reads as json:
/// the rollback default, so a pre-name db or a reverted binary keeps serving
/// the JSON leg.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Json,
    Sqlite,
}

impl Backend {
    pub fn name(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Sqlite => "sqlite",
        }
    }
}

/// The backend named RIGHT NOW: every keeper request and every settle call
/// re-reads it, so a backend change by another process lands on the next
/// request without a restart. Read-only: never creates graph.db, and an
/// absent db or table reads as json.
pub fn backend(graph: &Path) -> Backend {
    let connection = match Connection::open_with_flags(
        database_path(graph),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        Ok(connection) => connection,
        Err(_) => return Backend::Json,
    };
    match meta(&connection, "backend") {
        Ok(Some(value)) if value == Backend::Sqlite.name() => Backend::Sqlite,
        _ => Backend::Json,
    }
}

/// The store backend: sqlite only. The json leg is deleted. The name stays
/// so the `graph_meta.backend` row and the status verb keep one spelling.
pub const BACKEND_NAME: &str = "sqlite";

/// Stamp the sqlite backend onto a store, once. The first call also stamps
/// `backend_since_ms`; a re-run keeps the original clock, so the `--status`
/// days count never resets under an idempotent verb. The read, the writes,
/// and the commit run in one IMMEDIATE transaction, so concurrent callers
/// serialize: the second sees the first's rows and keeps its stamp.
/// Returns `(previous backend row, since stamp)` for the keeper receipt.
pub fn set_backend(graph: &Path) -> Result<(Option<String>, Option<u128>), String> {
    let mut connection = open(graph)?;
    let transaction = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let previous = meta(&transaction, "backend")?;
    stamp_meta(&transaction, "backend", BACKEND_NAME)?;
    let since = if previous.is_some() {
        meta(&transaction, "backend_since_ms")?.and_then(|value| value.parse::<u128>().ok())
    } else {
        let stamp = now_ms();
        stamp_meta(&transaction, "backend_since_ms", &stamp.to_string())?;
        Some(stamp)
    };
    transaction.commit().map_err(|error| error.to_string())?;
    Ok((previous, since))
}

/// A connection for a WRITE path: identical to [`open`], kept as a distinct
/// name so the table-ownership scan can tell read-only opens from writes.
pub(crate) fn write_connection(graph: &Path) -> Result<Connection, String> {
    open(graph)
}

pub(crate) fn open(graph: &Path) -> Result<Connection, String> {
    let path = database_path(graph);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let mut connection = Connection::open(&path).map_err(|error| error.to_string())?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(|error| error.to_string())?;
    connection
        .execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;
             PRAGMA foreign_keys=ON;
             PRAGMA recursive_triggers=ON;
             CREATE TABLE IF NOT EXISTS graph_meta (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );",
        )
        .map_err(|error| error.to_string())?;
    nodes::ensure_table(&connection)?;
    sessions::ensure_table(&connection)?;
    comments::ensure_table(&connection)?;
    encounters::ensure_table(&connection)?;
    pull_requests::ensure_table(&connection)?;
    relations::ensure_table(&connection)?;
    search::ensure_table(&connection)?;
    import_if_needed(&mut connection, graph)?;
    decisions::ensure_table(&connection)?;
    decisions::import_if_needed(&mut connection, graph)?;
    archive_import_if_needed(&mut connection, graph)?;
    Ok(connection)
}

/// The one-shot import: a db holding the blob `entries` table and no
/// `nodes` imports its rows into the owned tables, drops `entries`, and
/// stamps the current schema_version. A path with graph.json and no
/// graph.db imports on first open, which keeps the hundreds of test files
/// that seed graph.json fixtures working. A populated store re-imports
/// never; a schema-2 store migrates in place once (see
/// [`rebuild_if_schema_v2`]).
fn import_if_needed(connection: &mut Connection, graph: &Path) -> Result<(), String> {
    let nodes_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0))
        .map_err(|error| error.to_string())?;
    if nodes_count > 0 {
        return rebuild_if_schema_v2(connection);
    }
    let has_entries: bool = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'entries'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|count| count > 0)
        .map_err(|error| error.to_string())?;
    let mut rows: Vec<Value> = Vec::new();
    if has_entries {
        let mut statement = connection
            .prepare("SELECT row FROM entries ORDER BY ordinal, id")
            .map_err(|error| error.to_string())?;
        let blob_rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| error.to_string())?;
        for row in blob_rows {
            let body = row.map_err(|error| error.to_string())?;
            rows.push(
                serde_json::from_str(&body)
                    .map_err(|error| format!("entries row is invalid JSON: {error}"))?,
            );
        }
    } else if graph.exists() {
        let text = std::fs::read_to_string(graph).map_err(|error| error.to_string())?;
        if !text.trim().is_empty() {
            let doc: Value = serde_json::from_str(&text)
                .map_err(|error| format!("{} is invalid JSON: {error}", graph.display()))?;
            if let Some(entries) = doc.get("entries").and_then(Value::as_array) {
                rows = entries.clone();
            }
        }
    } else {
        // No seed file: the store starts empty. Stamp the version meta now
        // because the export path refuses a versionless store and no later
        // step stamps one for an empty graph.
        stamp_meta(connection, "schema_version", SCHEMA_VERSION)?;
        stamp_version_fields(connection, &content_version(&[]))?;
        return Ok(());
    }
    if rows.is_empty() && !has_entries {
        // An empty seed graph with no blob: same initial stamp as the
        // absent-file case; the first write bumps the counter.
        stamp_meta(connection, "schema_version", SCHEMA_VERSION)?;
        stamp_version_fields(connection, &content_version(&[]))?;
        return Ok(());
    }
    // Derive every row's defaults first, then dedup the slugs: two seed
    // rows carrying the same slug would collapse on the unique index and
    // silently lose the earlier row, which for a first import of a real
    // graph is row loss, not a quirk.
    for row in rows.iter_mut() {
        derive_store_defaults(row);
    }
    let mut taken: std::collections::HashSet<String> = Default::default();
    for row in rows.iter_mut() {
        let Some(obj) = row.as_object_mut() else {
            continue;
        };
        let mut slug = obj
            .get("slug")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if slug.is_empty() {
            continue;
        }
        if taken.contains(&slug) {
            let mut n = 2;
            while taken.contains(&format!("{slug}-{n}")) {
                n += 1;
            }
            slug = format!("{slug}-{n}");
            obj.insert("slug".to_string(), Value::String(slug.clone()));
        }
        taken.insert(slug);
    }
    let transaction = connection
        .transaction()
        .map_err(|error| error.to_string())?;
    for (ordinal, row) in rows.iter_mut().enumerate() {
        // A row still the model cannot represent after the derivation (an
        // id-less row, a status word that parses to nothing) is skipped,
        // not fatal: the skipped row stays only in the seed file.
        let Ok(mut node) = Node::from_json(row) else {
            continue;
        };
        node.ordinal = ordinal as i64;
        save_aggregate(&transaction, &node).map_err(|error| format!("import: {error}"))?;
    }
    if has_entries {
        transaction
            .execute("DROP TABLE entries", [])
            .map_err(|error| error.to_string())?;
    }
    // No recompute here: the import is a faithful fold of the seed, not a
    // write. Recomputing would re-derive seeded shapes (a done child would
    // roll its epic up, a stale claim mirror would hold in_progress) behind
    // the operator's back; the next write derives as usual.
    stamp_version_fields(&transaction, &content_version(&rows))?;
    transaction
        .execute(
            "INSERT INTO graph_meta(key, value) VALUES('schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![SCHEMA_VERSION],
        )
        .map_err(|error| error.to_string())?;
    transaction.commit().map_err(|error| error.to_string())?;
    Ok(())
}

/// The one-shot archive import: a sibling graph-archive.json folds its
/// entries into the same tables with `archived_at` stamped (the row's own
/// stamp, else the import time). An id already present in `nodes` SKIPS the
/// archived copy - the live row wins - and every skipped id is named on the
/// keeper's stderr, as is an unreadable or torn archive file, so rows that
/// never folded stay visible on every open.
fn archive_import_if_needed(connection: &mut Connection, graph: &Path) -> Result<(), String> {
    // v2: the v1 stamp fired on every fresh store whether or not an archive
    // file existed, so a store poisoned that way never folds a later-restored
    // file. The new key voids the v1 stamp: a store marked only by v1
    // re-runs the fold here.
    if meta(connection, "archive_imported_v2")?.is_some() {
        return Ok(());
    }
    let archive = graph.with_file_name("graph-archive.json");
    let mut rows: Vec<Value> = Vec::new();
    let mut read_a_file = false;
    if archive.exists() {
        // The file is an advisory export an old binary left behind: the
        // residents themselves live in this store, so unreadable or torn
        // bytes are dead weight to skip, never an incident - but the skip
        // is LOUD, because silent dead weight is how thousands of rows
        // stay folded out forever.
        match std::fs::read_to_string(&archive) {
            Ok(text) => match serde_json::from_str::<Value>(&text) {
                Ok(document) => {
                    rows = document
                        .get("entries")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    read_a_file = true;
                }
                Err(error) => eprintln!(
                    "warning: archive import: {} did not parse ({}); its rows are NOT folded",
                    archive.display(),
                    error
                ),
            },
            Err(error) => eprintln!(
                "warning: archive import: {} could not be read ({}); its rows are NOT folded",
                archive.display(),
                error
            ),
        }
    }
    let transaction = connection
        .transaction()
        .map_err(|error| error.to_string())?;
    let mut next_ordinal: i64 = transaction
        .query_row("SELECT COALESCE(MAX(ordinal), -1) FROM nodes", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|error| error.to_string())?
        + 1;
    let mut reused_ids: Vec<&str> = Vec::new();
    for row in &rows {
        let Some(id) = row.get("id").and_then(Value::as_str) else {
            continue;
        };
        let exists: i64 = transaction
            .query_row(
                "SELECT COUNT(*) FROM nodes WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if exists > 0 {
            // A reused id (an old done archive row and a different live node
            // minted after the move) skips the archived copy: the live row is
            // the authority and the fold must complete, not refuse. The
            // archived copy stays in the file, and the skip is named so the
            // divergence between file and store stays visible.
            reused_ids.push(id);
            continue;
        }
        let Ok(mut node) = Node::from_json(row) else {
            continue;
        };
        node.ordinal = next_ordinal;
        next_ordinal += 1;
        if node.archived_at.is_none() {
            node.archived_at = Some(crate::graph_store::now_isoformat());
        }
        save_aggregate(&transaction, &node).map_err(|error| format!("archive import: {error}"))?;
    }
    // Only a store that actually read an archive file marks the fold done:
    // a store with no file (or an unreadable one) must re-probe on the next
    // spawn so a later-restored archive still imports.
    if read_a_file {
        stamp_meta(&transaction, "archive_imported_v2", "1")?;
    }
    if !reused_ids.is_empty() {
        eprintln!(
            "warning: archive import: {} id(s) skipped, already live in nodes: {}",
            reused_ids.len(),
            reused_ids.join(", ")
        );
    }
    transaction.commit().map_err(|error| error.to_string())
}

/// Schema 3: the store rows are the only authority, so a schema-2 store
/// needs no data rewrite; this migration drops the retired soak sampler's
/// graph_meta keys and stamps the version. The seed-graph.json rebuild this
/// migration once ran for json-backed stores is gone with the json leg:
/// rewriting live rows from the stale seed would clobber real mutations.
fn rebuild_if_schema_v2(connection: &mut Connection) -> Result<(), String> {
    let target: i64 = SCHEMA_VERSION.parse().unwrap_or(i64::MAX);
    let current: i64 = meta(connection, "schema_version")?
        .and_then(|raw| raw.parse::<i64>().ok())
        .unwrap_or(2);
    if current >= target {
        return Ok(());
    }
    connection
        .execute(
            "DELETE FROM graph_meta WHERE key IN ('soak_clean_since_ms', 'soak_clean_days',
             'soak_last_sample_ms', 'soak_last_divergent')",
            [],
        )
        .map_err(|error| error.to_string())?;
    // A store that reached v3 with rows but no version meta (a raw-SQL
    // seed, a pre-counter store) gets its initial stamp here: the export
    // path refuses a versionless store, and the version is the content
    // digest of exactly the rows being migrated.
    if meta(connection, "version")?.is_none() {
        let version = content_version(&export_rows_of(connection)?);
        stamp_version_fields(connection, &version)?;
    }
    stamp_meta(connection, "schema_version", SCHEMA_VERSION)
}

fn stamp_version_fields(connection: &Connection, version: &str) -> Result<(), String> {
    connection
        .execute(
            "INSERT INTO graph_meta(key, value) VALUES('version', ?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![version],
        )
        .map_err(|error| error.to_string())?;
    let stamped = now_ms().to_string();
    connection
        .execute(
            "INSERT INTO graph_meta(key, value) VALUES('updated_ms', ?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![stamped],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// The write finalizer: stamp the content digest AND move the typed API's
/// mutation counter by one. The lazy import does NOT take this path (a
/// backfill is not a user-visible write; AC15 counts one bump per
/// mutation), so it stamps fields only.
pub(crate) fn stamp_version(connection: &Connection, version: &str) -> Result<(), String> {
    stamp_version_fields(connection, version)?;
    bump_api_version(connection)?;
    Ok(())
}

/// Bump the typed API's mutation counter by one, in the caller's
/// transaction, and return the new value. Every store write passes through
/// `stamp_version`, so legacy writers (a note, an op) move the counter too:
/// `fno backlog version` grows by one across any single write.
fn bump_api_version(connection: &Connection) -> Result<(), String> {
    connection
        .execute(
            "INSERT INTO graph_meta(key, value) VALUES('api_version', '1')
             ON CONFLICT(key) DO UPDATE SET
             value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)",
            [],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// The counter `backlog::api::version` serves. It lives in graph_meta of the
/// relational shadow, which both backends write, so the counter survives the
/// flip. Read-only probe: an absent db or key reads 0, never creates.
pub fn api_version(graph: &Path) -> Result<i64, String> {
    if !database_path(graph).exists() {
        return Ok(0);
    }
    let connection = Connection::open_with_flags(
        database_path(graph),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|error| error.to_string())?;
    match meta(&connection, "api_version") {
        Ok(Some(value)) => value
            .parse::<i64>()
            .map_err(|error| format!("api_version is not an integer: {error}")),
        Ok(None) => Ok(0),
        // A db created but not yet committed (another thread or process is
        // inside open()'s DDL) has no graph_meta yet: the probe reads 0, the
        // same answer an absent db gives, and the creator's commit lands on
        // the next read.
        Err(error) if error.contains("no such table") => Ok(0),
        Err(error) => Err(error),
    }
}

pub(crate) fn stamp_meta(connection: &Connection, key: &str, value: &str) -> Result<(), String> {
    connection
        .execute(
            "INSERT INTO graph_meta(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// The last store version the canonical view pass rendered (graph_meta of
/// the shadow db, which both backends maintain). `None` = nothing rendered
/// since the counter was born, so the next settled trigger owes a render.
pub fn rendered_version(graph: &Path) -> Result<Option<String>, String> {
    if !database_path(graph).exists() {
        return Ok(None);
    }
    let connection = open(graph)?;
    meta(&connection, "rendered_version")
}

/// Stamp the rendered marker. The render trigger's own bookkeeping: it runs
/// on the keeper's render thread, outside any mutation, so this writes meta
/// directly, the same lane `export_now`'s stamp uses.
pub fn set_rendered_version(graph: &Path, value: &str) -> Result<(), String> {
    let connection = open(graph)?;
    stamp_meta(&connection, "rendered_version", value)
}

/// The backend-owned write: same rows, but graph_meta.version is the
/// store's own content digest, and the digest is returned.
pub fn authoritative_sync(
    graph: &Path,
    before: &[Value],
    after: &[Value],
) -> Result<String, String> {
    let mut connection = open(graph)?;
    let transaction = connection
        .transaction()
        .map_err(|error| error.to_string())?;
    let _report = write_changed(&transaction, before, after, true)?;
    let version = content_version(after);
    stamp_version(&transaction, &version)?;
    confirm_ids_landed(&transaction, after)?;
    transaction.commit().map_err(|error| error.to_string())?;
    Ok(version)
}

pub(crate) struct WriteReport {
    present_ids: Vec<String>,
    deleted_ids: Vec<String>,
}

fn confirm_ids_landed(connection: &Connection, after: &[Value]) -> Result<(), String> {
    const SQLITE_BIND_BATCH: usize = 900;
    let expected: std::collections::BTreeSet<String> = after
        .iter()
        .filter_map(crate::graph_store::entry_id)
        .map(str::to_owned)
        .collect();
    let stored_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0))
        .map_err(|error| error.to_string())?;

    let mut stored = std::collections::BTreeSet::new();
    let ids: Vec<&str> = expected.iter().map(String::as_str).collect();
    for batch in ids.chunks(SQLITE_BIND_BATCH) {
        let placeholders = std::iter::repeat_n("?", batch.len())
            .collect::<Vec<_>>()
            .join(",");
        let query = format!("SELECT id FROM nodes WHERE id IN ({placeholders})");
        let mut statement = connection
            .prepare(&query)
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(batch.iter().copied()), |row| {
                row.get::<_, String>(0)
            })
            .map_err(|error| error.to_string())?;
        for row in rows {
            stored.insert(row.map_err(|error| error.to_string())?);
        }
    }
    let missing: Vec<&str> = expected
        .iter()
        .map(String::as_str)
        .filter(|id| !stored.contains(*id))
        .collect();
    if stored_count == expected.len() as i64 && missing.is_empty() {
        return Ok(());
    }
    Err(format!(
        "publish read-back mismatch: expected {} ids, stored {stored_count}; missing ids {missing:?}",
        expected.len()
    ))
}

/// The single-row mutation path: under the sqlite backend,
/// one `BEGIN IMMEDIATE` transaction reads the CURRENT authoritative rows,
/// applies the mutation, writes only the changed nodes' aggregates, runs the
/// single-row status recompute, and stamps a fresh content version. The
/// immediate transaction takes the write lock up front, so the read is never
/// a stale snapshot: a concurrent writer either landed before our lock or
/// waits behind it, and both rows persist. A busy timeout retries the whole
/// cycle instead of surfacing as a refusal.
pub fn mutate_single_row(
    graph: &Path,
    mutation: &str,
    mut apply: impl FnMut(&mut Vec<Value>) -> Result<bool, String>,
) -> Result<bool, String> {
    const ATTEMPTS: usize = 3;
    let started = std::time::Instant::now();
    let mut retries = 0u32;
    for attempt in 0..ATTEMPTS {
        match mutate_single_row_once(graph, mutation, &mut apply) {
            Ok(outcome) => {
                if outcome {
                    emit_gate_event(mutation, started.elapsed().as_millis(), retries);
                }
                return Ok(outcome);
            }
            Err(error) => {
                let busy = error.contains("locked") || error.contains("busy");
                if busy && attempt + 1 < ATTEMPTS {
                    retries += 1;
                    std::thread::sleep(Duration::from_millis(100 * (attempt as u64 + 1)));
                    continue;
                }
                return Err(error);
            }
        }
    }
    unreachable!("retry loop returns on every branch")
}

/// The raw row publish behind the keeper's `commit_rows` method: the client
/// mutated a snapshot it read from this store and ships the rows that
/// changed plus the ids it removed. Changed rows replace by id (or append,
/// in the order shipped, when the id is new); removed ids drop. Returns the
/// post-write rows so the caller can render and answer in the same breath.
pub fn apply_client_rows(
    graph: &Path,
    mutation: &str,
    mut changed: Vec<Value>,
    removed: Vec<String>,
    expected_version: Option<&str>,
) -> Result<Vec<Value>, String> {
    mutate_single_row(graph, mutation, move |rows: &mut Vec<Value>| {
        // The optimistic-token check INSIDE the write transaction: a
        // pre-transaction `state_version` probe leaves a window where another
        // writer lands between check and commit, and the tx would silently
        // overwrite its rows (measured: 53 of 100 concurrent same-row notes
        // survived). The tx's own authoritative rows are what the client's
        // base_version must still name.
        if let Some(expected) = expected_version {
            if content_version(rows) != expected {
                return Err("graph conflict: base version moved".into());
            }
        }
        rows.retain(|row| {
            crate::graph_store::entry_id(row).map_or(true, |id| !removed.contains(&id.to_string()))
        });
        for row in rows.iter_mut() {
            let Some(id) = crate::graph_store::entry_id(row).map(str::to_string) else {
                continue;
            };
            if let Some(pos) = changed
                .iter()
                .position(|c| crate::graph_store::entry_id(c) == Some(id.as_str()))
            {
                *row = changed.remove(pos);
            }
        }
        let known: std::collections::HashSet<String> = rows
            .iter()
            .filter_map(|r| crate::graph_store::entry_id(r).map(str::to_string))
            .collect();
        let fresh: Vec<Value> = changed
            .iter()
            .filter(|c| crate::graph_store::entry_id(c).map_or(false, |id| !known.contains(id)))
            .cloned()
            .collect();
        rows.extend(fresh);
        Ok(true)
    })?;
    read_entries(graph)
}

fn mutate_single_row_once(
    graph: &Path,
    mutation: &str,
    apply: &mut dyn FnMut(&mut Vec<Value>) -> Result<bool, String>,
) -> Result<bool, String> {
    let _ = mutation;
    let mut connection = open(graph)?;
    let transaction = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    // The rows INSIDE the write transaction are the authoritative state:
    // never a cutover snapshot, never the JSON export.
    let rows = export_rows(&transaction)?;
    let mut working = rows.clone();
    if !apply(&mut working)? {
        // Domain refusal: the transaction drops here, nothing is written.
        return Ok(false);
    }
    // The publish-seam invariants the whole-graph path ran on every publish
    // hold here too: no empty presence field, a slug on every row, and a
    // touched_at stamp when a row's curation fields moved.
    for row in working.iter() {
        let Some(obj) = row.as_object() else {
            continue;
        };
        let id = crate::graph_store::entry_id(row).unwrap_or("<no id>");
        for field in crate::graph_store::PRESENCE_TEXT_FIELDS {
            if let Some(serde_json::Value::String(text)) = obj.get(*field) {
                if text.trim().is_empty() {
                    return Err(format!(
                        "refusing to persist an empty '{field}' on entry '{id}': \
                         pass real content, or remove the key to clear it"
                    ));
                }
            }
        }
    }
    crate::graph_store::ensure_slugs(&mut working);
    let now_iso = crate::graph_store::now_isoformat();
    for row in working.iter_mut() {
        let (Some(id), true) = (
            crate::graph_store::entry_id(row).map(str::to_string),
            row.is_object(),
        ) else {
            continue;
        };
        let Some(before) = rows
            .iter()
            .find(|r| crate::graph_store::entry_id(r) == Some(id.as_str()))
        else {
            continue; // absent from the pre-image: new node, created_at carries it
        };
        // curation_key reads the curation fields only, so a stamped
        // touched_at can never cancel its own trigger.
        if crate::graph_store::curation_key(row) != crate::graph_store::curation_key(before) {
            row.as_object_mut().unwrap().insert(
                "touched_at".to_string(),
                serde_json::Value::String(now_iso.clone()),
            );
        }
    }
    write_changed(&transaction, &rows, &working, true)?;
    nodes::recompute_status(&transaction)?;
    let rows_after = export_rows(&transaction)?;
    let version = content_version(&rows_after);
    stamp_version(&transaction, &version)?;
    transaction.commit().map_err(|error| error.to_string())?;
    Ok(true)
}

/// The `graph_write_gate` row the single-row path emits after each landed
/// mutation. It carries the required window fields as honest point values
/// (a zero-length window covering the write) plus the `mutation` name, so
/// AC24 is measurable and the port-bar audit can tell these rows from the
/// keeper's five-minute windows.
pub(crate) fn emit_gate_event(mutation: &str, wait_ms: u128, retries: u32) {
    let now = now_ms() as i64;
    // Best-effort: an undeclared state root (a hermetic test with no pins)
    // skips the emission instead of failing the mutation that earned it.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let Some(space) = crate::paths::space_dir_opt(&cwd) else {
        return;
    };
    let path = space.join("events.jsonl");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let emitter = crate::events::EventEmitter::new(path, "backlog");
    let _ = emitter.emit(
        "graph_write_gate",
        &serde_json::json!({
            "keeper_pid": std::process::id(),
            "window_started_ms": now,
            "window_finished_ms": now,
            "completed_window_seconds": 0,
            "wait_ms_bounds": [wait_ms as i64],
            "wait_ms_counts": [1],
            "mutation_count": 1,
            "bytes_written": 0,
            "retry_count": retries,
            "mutation": mutation,
        }),
    );
}

/// Save one node's whole row set through the owning modules. The caller
/// owns the transaction; only this node's rows are touched.
fn save_aggregate(connection: &Connection, node: &Node) -> Result<(), String> {
    nodes::save(connection, node)?;
    sessions::save(
        connection,
        &node.id,
        node.sessions.as_deref().unwrap_or(&[]),
    )?;
    comments::save(
        connection,
        &node.id,
        node.comments.as_deref().unwrap_or(&[]),
    )?;
    encounters::save(
        connection,
        &node.id,
        node.encounters.as_deref().unwrap_or(&[]),
    )?;
    let mut prs: Vec<crate::backlog::model::PullRequest> = Vec::new();
    if let Some(primary) = &node.primary_pr {
        prs.push(primary.clone());
    }
    prs.extend(node.additional_prs.iter().flatten().cloned());
    pull_requests::save(connection, &node.id, &prs)?;
    relations::save(connection, &node.id, &node.relations)?;
    Ok(())
}

/// Delete one node's whole row set through the owning modules.
fn delete_aggregate(connection: &Connection, id: &str) -> Result<(), String> {
    nodes::delete(connection, id)?;
    sessions::delete(connection, id)?;
    comments::delete(connection, id)?;
    encounters::delete(connection, id)?;
    pull_requests::delete(connection, id)?;
    relations::delete(connection, id)?;
    Ok(())
}

/// Write only the changed nodes. The unit of change is the node: a row set
/// (nodes row, its single-row mirrors, its child tables) is replaced for
/// each id whose canonical JSON differs.
///
/// Returns the number of ids acted on (saved or deleted), so a test can
/// count what a sync moved.
/// Fill in what a minimal row never carried, so it represents: the title
/// and slug derive from the title or the id, and kind/priority/status take
/// the write path's defaults (feature, p2, idea). One derivation for the
/// import and the row-commit path, so a row cannot persist through one and
/// drop through the other.
fn derive_store_defaults(row: &mut Value) {
    let Some(obj) = row.as_object_mut() else {
        return;
    };
    // The legacy folds the whole-graph defaults pass performs: a `_status`
    // KEY is the pre-rename spelling of the status VALUE, and a couple of
    // status/priority words predate a rename. Folding them here keeps a
    // legacy seed reading identically on the import and commit paths.
    // The pre-rename KEY spelling is not a modeled field: drop it, and let
    // the status default. Only a literal `status` value ever renames.
    obj.shift_remove("_status");
    if let Some(old) = obj.get("priority").and_then(Value::as_str) {
        if let Some((_, to)) = crate::graph_store::PRIORITY_MIGRATION
            .iter()
            .find(|(from, _)| *from == old)
        {
            obj.insert("priority".to_string(), Value::String(to.to_string()));
        }
    }
    if let Some(old) = obj.get("status").and_then(Value::as_str) {
        if let Some((_, to)) = crate::graph_store::STATUS_MIGRATION
            .iter()
            .find(|(from, _)| *from == old)
        {
            obj.insert("status".to_string(), Value::String(to.to_string()));
        }
    }
    let title = obj
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if title.is_empty() {
        let fallback = obj
            .get("slug")
            .and_then(Value::as_str)
            .or_else(|| obj.get("id").and_then(Value::as_str))
            .unwrap_or("")
            .to_string();
        obj.insert("title".to_string(), Value::String(fallback));
    }
    let slug = obj.get("slug").and_then(Value::as_str).unwrap_or("");
    if slug.is_empty() {
        let title = obj.get("title").and_then(Value::as_str).unwrap_or("");
        obj.insert(
            "slug".to_string(),
            Value::String(crate::graph_store::derive_base_slug(title)),
        );
    }
    obj.entry("type".to_string())
        .or_insert(Value::String("feature".to_string()));
    obj.entry("priority".to_string())
        .or_insert(Value::String("p2".to_string()));
    obj.entry("status".to_string())
        .or_insert(Value::String("idea".to_string()));
}

pub(crate) fn write_changed(
    connection: &Connection,
    before: &[Value],
    after: &[Value],
    strict: bool,
) -> Result<WriteReport, String> {
    if strict {
        let mut seen_ids = std::collections::BTreeSet::new();
        for (ordinal, row) in after.iter().enumerate() {
            let Some(id) = row.get("id").and_then(Value::as_str) else {
                return Err(format!(
                    "row at ordinal {ordinal} is unrepresentable: missing string id"
                ));
            };
            if id.is_empty() {
                return Err(format!(
                    "row at ordinal {ordinal} is unrepresentable: empty string id"
                ));
            }
            if !seen_ids.insert(id) {
                return Err(format!("duplicate id in after rows: {id}"));
            }
        }
    }

    fn by_id(rows: &[Value]) -> std::collections::BTreeMap<String, &Value> {
        rows.iter()
            .filter(|row| row.is_object())
            .filter_map(|row| crate::graph_store::entry_id(row).map(|id| (id.to_string(), row)))
            .collect()
    }
    let before_map = by_id(before);
    let after_map = by_id(after);
    let ordinals: std::collections::BTreeMap<&str, i64> = after
        .iter()
        .enumerate()
        .filter_map(|(ordinal, row)| {
            crate::graph_store::entry_id(row).map(|id| (id, ordinal as i64))
        })
        .collect();
    let mut ids: Vec<String> = before_map.keys().chain(after_map.keys()).cloned().collect();
    ids.sort();
    ids.dedup();
    let mut report = WriteReport {
        present_ids: Vec::new(),
        deleted_ids: Vec::new(),
    };
    for id in ids {
        let old = before_map.get(&id);
        let new = after_map.get(&id);
        let unchanged = match (old, new) {
            // Fast path: equal raw serializations are the same row, so only
            // unequal raw strings pay for the canonical form. The canonical
            // compare is the load-bearing one on the sqlite branch: its raw
            // export omits nulls, so a plain string compare there would
            // rewrite every row on every mutation.
            // ponytail: whole-graph canonical compare, removed when
            // single-row writes land.
            (Some(b), Some(a)) => {
                crate::graph_store::to_python_json(b) == crate::graph_store::to_python_json(a)
                    || canonical_row(b) == canonical_row(a)
            }
            (None, None) => true,
            _ => false,
        };
        if unchanged {
            continue;
        }
        match new {
            Some(body) => {
                // A row may name only what its writer knew: derive the same
                // defaults the import applies so a minimal new row
                // persists instead of silently dropping (a creator verb
                // that ships no status key would otherwise vanish).
                let mut body = (*body).clone();
                derive_store_defaults(&mut body);
                let mut node = match Node::from_json(&body) {
                    Ok(node) => node,
                    Err(error) if strict => {
                        return Err(format!("row {id} is unrepresentable: {error}"));
                    }
                    Err(_) => continue,
                };
                node.ordinal = ordinals.get(id.as_str()).copied().unwrap_or(0);
                save_aggregate(connection, &node)?;
                report.present_ids.push(id);
            }
            None => {
                delete_aggregate(connection, &id)?;
                report.deleted_ids.push(id);
            }
        }
    }
    Ok(report)
}

/// Every stored node, in ordinal order, as its canonical JSON row. This is
/// the read surface every store consumer shares.
pub fn read_entries(graph: &Path) -> Result<Vec<Value>, String> {
    let connection = open(graph)?;
    export_rows(&connection)
}

/// The rows behind an open connection, in ordinal order. The version guard
/// refuses a store the import never stamped; the migrator reads rows
/// through `export_rows_of` instead, because it is the code that stamps.
pub fn export_rows(connection: &Connection) -> Result<Vec<Value>, String> {
    if meta(connection, "version")?.is_none() {
        return Err("SQLite graph has no version".into());
    }
    export_rows_of(connection)
}

/// The export without the version guard, for the import/migration paths
/// that run before (or while) the version meta exists.
pub(crate) fn export_rows_of(connection: &Connection) -> Result<Vec<Value>, String> {
    let mut statement = connection
        .prepare("SELECT id FROM nodes ORDER BY ordinal, id")
        .map_err(|error| error.to_string())?;
    let ids = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?;
    let mut entries = Vec::new();
    for id in ids {
        let id = id.map_err(|error| error.to_string())?;
        let Some(node) = nodes::load(&connection, &id)? else {
            return Err(format!("node {id} vanished mid-export"));
        };
        entries.push(node.to_json());
    }
    Ok(entries)
}

pub(crate) fn meta(connection: &Connection, key: &str) -> Result<Option<String>, String> {
    connection
        .query_row(
            "SELECT value FROM graph_meta WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())
}

pub fn version(graph: &Path) -> Result<String, String> {
    let connection = open(graph)?;
    meta(&connection, "version")?.ok_or_else(|| "SQLite graph has no version".into())
}

pub fn export_status(graph: &Path) -> Result<(String, Option<String>), String> {
    let connection = open(graph)?;
    let current =
        meta(&connection, "version")?.ok_or_else(|| "SQLite graph has no version".to_string())?;
    Ok((current, meta(&connection, "exported_version")?))
}

pub fn export_now(graph: &Path) -> Result<String, String> {
    let entries = read_entries(graph)?;
    let version = content_version(&entries);
    crate::graph_store::write_atomic(graph, &crate::graph_store::serialize_graph_file(&entries))
        .map_err(|error| error.to_string())?;
    // Best-effort: the JSON write is the deliverable (AC21's exit code reads
    // it); a failed snapshot still leaves graph.db itself (WAL) in place.
    let _ = snapshot_db(graph, now_ms());
    let connection = open(graph)?;
    let stamped = now_ms().to_string();
    connection
        .execute(
            "INSERT INTO graph_meta(key, value) VALUES('exported_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![version],
        )
        .map_err(|error| error.to_string())?;
    connection
        .execute(
            "INSERT INTO graph_meta(key, value) VALUES('last_export_ms', ?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![stamped],
        )
        .map_err(|error| error.to_string())?;
    Ok(version)
}

/// When the current backend took over, in unix ms. A read-only probe: an
/// absent db or key reads None, never creates.
pub fn backend_since(graph: &Path) -> Result<Option<u128>, String> {
    if !database_path(graph).exists() {
        return Ok(None);
    }
    let connection = Connection::open_with_flags(
        database_path(graph),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|error| error.to_string())?;
    match meta(&connection, "backend_since_ms") {
        Ok(Some(value)) => value
            .parse::<u128>()
            .map(Some)
            .map_err(|error| format!("backend_since_ms is not an integer: {error}")),
        Ok(None) => Ok(None),
        Err(error) if error.contains("no such table") => Ok(None),
        Err(error) => Err(error),
    }
}

/// At most one `graph.db.<stamp>` snapshot per hour in `backups/`, keeping
/// the newest [`crate::graph_store::GRAPH_BACKUP_KEEP`]. Replaces the
/// per-export graph.json copy (task 10.1): after the flip graph.json is an
/// on-demand artifact and copying 14.6 MB per export recreates the write
/// volume the flip deletes. VACUUM INTO also compacts; its target must not
/// exist, so the microsecond stamp names it. The hour gate lives in
/// `graph_meta.last_snapshot_ms`, not file mtimes, so a moved or inspected
/// snapshot cannot skew the clock.
fn snapshot_db(graph: &Path, now: u128) -> Result<(), String> {
    let connection = open(graph)?;
    if let Some(last) = meta(&connection, "last_snapshot_ms")? {
        let last: u128 = last
            .parse()
            .map_err(|error| format!("last_snapshot_ms is not an integer: {error}"))?;
        if now.saturating_sub(last) < Duration::from_secs(3600).as_millis() {
            return Ok(());
        }
    }
    let parent = graph
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", graph.display()))?;
    let dir = parent.join("backups");
    std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let target = dir.join(format!("graph.db.{}", crate::graph_store::backup_stamp()));
    connection
        .execute("VACUUM INTO ?1", params![target.display().to_string()])
        .map_err(|error| format!("snapshot {}: {error}", target.display()))?;
    stamp_meta(&connection, "last_snapshot_ms", &now.to_string())?;
    let _ = crate::graph_store::rotate_backups(&dir, "graph.db.");
    Ok(())
}

/// Recursively sort object keys so a row's key order never decides
/// equality: an imported fixture and a store row can carry the same fields
/// in a different order, and only the sorted form compares equal.
fn sorted_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: std::collections::BTreeMap<String, Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), sorted_value(v)))
                .collect();
            serde_json::to_value(&sorted).unwrap_or(Value::Null)
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted_value).collect()),
        _ => value.clone(),
    }
}

/// One row's canonical JSON: null-stripped, key-sorted, Python-spaced. The
/// equality rule `write_changed` uses to decide whether a row changed.
fn canonical_row(row: &Value) -> String {
    crate::graph_store::to_python_json(&sorted_value(&crate::backlog::model::strip_nulls_value(
        row,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fixture(name: &str) -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let graph = dir.path().join(name);
        std::fs::write(&graph, b"{\"entries\": []}").unwrap();
        (dir, graph)
    }

    #[test]
    fn vacuum_snapshot_once_per_hour_and_prunes() {
        let dir = TempDir::new().unwrap();
        let graph = two_node_graph(&dir);
        let now = now_ms();
        let snaps = |d: &Path| -> Vec<PathBuf> {
            let mut paths: Vec<PathBuf> = std::fs::read_dir(d)
                .unwrap()
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| {
                    path.file_name()
                        .map(|name| name.to_string_lossy().starts_with("graph.db."))
                        .unwrap_or(false)
                })
                .collect();
            paths.sort();
            paths
        };
        let backup_dir = graph.parent().unwrap().join("backups");
        std::fs::create_dir_all(&backup_dir).unwrap();
        snapshot_db(&graph, now).unwrap();
        snapshot_db(&graph, now + 60_000).unwrap();
        assert_eq!(snaps(&backup_dir).len(), 1, "one snapshot per hour");
        // After the hour gate a second lands; the stamp moves with it.
        snapshot_db(&graph, now + 3_600_001).unwrap();
        assert_eq!(snaps(&backup_dir).len(), 2);
        // Plant stale snapshots past the retention cap; the next hourly
        // snapshot prunes to GRAPH_BACKUP_KEEP.
        for i in 0..(crate::graph_store::GRAPH_BACKUP_KEEP + 3) {
            std::fs::write(backup_dir.join(format!("graph.db.old{i}")), b"x").unwrap();
        }
        snapshot_db(&graph, now + 2 * 3_600_002).unwrap();
        assert_eq!(
            snaps(&backup_dir).len(),
            crate::graph_store::GRAPH_BACKUP_KEEP,
            "retention prunes to GRAPH_BACKUP_KEEP"
        );
    }

    #[test]
    fn set_backend_stamps_since_once() {
        let (dir, graph) = fixture("graph.json");
        let (previous, since1) = set_backend(&graph).unwrap();
        assert_eq!(previous, None, "a fresh store has no backend row");
        let since1 = since1.expect("the first stamp sets the clock");
        // An idempotent re-run keeps the original clock.
        let (previous, since2) = set_backend(&graph).unwrap();
        assert_eq!(previous.as_deref(), Some(BACKEND_NAME));
        assert_eq!(since2, Some(since1), "a re-run never resets the clock");
        drop(dir);
    }

    fn two_node_graph(dir: &TempDir) -> PathBuf {
        let graph = dir.path().join("graph.json");
        std::fs::write(
            &graph,
            r#"{"entries": [
                {"id": "ab-one", "slug": "one", "title": "One", "type": "feature",
                 "status": "idea", "priority": "p2", "domain": "code",
                 "created_at": "2026-09-11T00:00:00+00:00", "tags": [],
                 "locked_by": "holder-1", "locked_at": "2026-09-11T01:00:00+00:00",
                 "dispatch_verb": "do",
                 "source": "idea", "source_kind": "operator_request",
                 "supersession": {"successor": "ab-two", "reason": "merged"},
                 "sessions": [{"phase": "do", "harness": "claude",
                               "session_id": "s-1"}],
                 "blocked_by": ["ab-two"]},
                {"id": "ab-two", "slug": "two", "title": "Two", "type": "bug",
                 "status": "ready", "priority": "p1", "domain": "code",
                 "created_at": "2026-09-11T00:00:00+00:00"}
            ]}"#,
        )
        .unwrap();
        graph
    }

    #[test]
    fn backlog_schema_import_on_first_open_keeps_fixture_graphs_working() {
        let dir = TempDir::new().unwrap();
        let graph = two_node_graph(&dir);
        let entries = read_entries(&graph).unwrap();
        assert_eq!(entries.len(), 2, "both rows imported");
        let connection = open(&graph).unwrap();
        let sessions: i64 = connection
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sessions, 1, "the session row imported");
        let relations: i64 = connection
            .query_row("SELECT COUNT(*) FROM relations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(relations, 1, "the blocked_by edge imported");
        let schema: String = connection
            .query_row(
                "SELECT value FROM graph_meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(schema, SCHEMA_VERSION);
    }

    #[test]
    fn backlog_schema_v2_import_drops_the_entries_blob() {
        // A wave-1 db: blob entries table, no nodes. Open imports and drops.
        let dir = TempDir::new().unwrap();
        let graph = dir.path().join("graph.json");
        std::fs::write(&graph, b"{\"entries\": []}").unwrap();
        let connection = open(&graph).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE entries (
                     id TEXT PRIMARY KEY, ordinal INTEGER NOT NULL, row TEXT NOT NULL);
                 INSERT INTO entries VALUES ('ab-old', 0, '{\"id\": \"ab-old\",
                     \"slug\": \"old\", \"title\": \"Old\", \"type\": \"feature\",
                     \"status\": \"done\", \"priority\": \"p2\", \"domain\": \"code\",
                     \"created_at\": \"2026-09-01T00:00:00+00:00\"}');",
            )
            .unwrap();
        drop(connection);
        let entries = read_entries(&graph).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["id"], "ab-old");
        let connection = open(&graph).unwrap();
        let has_entries: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'entries'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_entries, 0, "the blob table is dropped");
    }

    #[test]
    fn backlog_schema_shadow_write_touches_only_changed_nodes() {
        // AC10-HP: a mutation lands, only the changed node's rows move.
        let dir = TempDir::new().unwrap();
        let graph = two_node_graph(&dir);
        let before = read_entries(&graph).unwrap();
        let mut after = before.clone();
        after[0]
            .as_object_mut()
            .unwrap()
            .insert("title".to_string(), Value::String("One changed".into()));
        authoritative_sync(&graph, &before, &after).unwrap();

        let connection = open(&graph).unwrap();
        let reloaded = export_rows(&connection).unwrap();
        assert_eq!(reloaded[0]["title"], "One changed");
        assert_eq!(reloaded[1]["title"], "Two", "untouched node unchanged");
        // The changed node's session rows survived the rewrite.
        let sessions: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE node_id = 'ab-one'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sessions, 1);
        // The relation survived.
        let relations: i64 = connection
            .query_row("SELECT COUNT(*) FROM relations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(relations, 1);
    }

    #[test]
    fn backlog_schema_delete_removes_the_whole_aggregate() {
        let dir = TempDir::new().unwrap();
        let graph = two_node_graph(&dir);
        let before = read_entries(&graph).unwrap();
        let after: Vec<Value> = before[1..].to_vec();
        authoritative_sync(&graph, &before, &after).unwrap();
        let connection = open(&graph).unwrap();
        let nodes_left: i64 = connection
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(nodes_left, 1, "the surviving node stays");
        // The mirrors cascade with the node row; a pragma-less connection
        // would strand these as orphans.
        for table in [
            "node_claims",
            "node_dispatch",
            "node_provenance",
            "supersessions",
            "sessions",
            "relations",
        ] {
            let rows: i64 = connection
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(rows, 0, "{table} holds no rows for the deleted node");
        }
    }

    fn raw_rows(graph: &Path) -> Vec<Value> {
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(graph).unwrap()).unwrap();
        doc.get("entries")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    }

    #[test]
    fn flipgate_shadow_superseded_settle_reaches_the_store() {
        // AC2-HP: the raw row is blocked with superseded_by set; the
        // mutation pipeline settles it to superseded. The settle must land
        // in the store rows themselves.
        let dir = TempDir::new().unwrap();
        let graph = two_node_graph(&dir);
        let mut raw = raw_rows(&graph);
        // The pre-image: ab-two is blocked, superseded by ab-one.
        raw[1]
            .as_object_mut()
            .unwrap()
            .insert("superseded_by".to_string(), Value::String("ab-one".into()));
        raw[1]
            .as_object_mut()
            .unwrap()
            .insert("status".to_string(), Value::String("blocked".into()));
        raw[1].as_object_mut().unwrap().insert(
            "blocked_reason".to_string(),
            Value::String("pending supersession".into()),
        );
        authoritative_sync(&graph, &[], &raw).unwrap();
        let seeded = read_entries(&graph).unwrap();
        assert_eq!(
            seeded[1]["status"], "blocked",
            "the seed holds the pre-image"
        );
        // Mutate the OTHER node; the pipeline settles ab-two itself.
        let mut after = seeded.clone();
        after[0]
            .as_object_mut()
            .unwrap()
            .insert("title".to_string(), Value::String("One changed".into()));
        crate::graph_store::locked_mutate_with_hook(
            &graph,
            crate::graph_store::MutateInput {
                entries: after,
                canonical_path: None,
                base_version: version(&graph).unwrap(),
                plan_rungs: None,
            },
            std::time::Duration::from_secs(10),
            None,
        )
        .unwrap();
        let rows = read_entries(&graph).unwrap();
        assert_eq!(
            rows[1]["status"], "superseded",
            "the settle reached the store"
        );
    }

    #[test]
    fn flipgate_shadow_write_changed_counts_only_canonical_changes() {
        // AC3-EDGE: rows that differ only by explicit nulls write nothing.
        let dir = TempDir::new().unwrap();
        let graph = two_node_graph(&dir);
        let before = raw_rows(&graph);
        let mut after = before.clone();
        // One real change.
        after[0]
            .as_object_mut()
            .unwrap()
            .insert("title".to_string(), Value::String("One changed".into()));
        // One null-only difference on the row that has no title key... the
        // two-node fixture's ab-two HAS a title, so drop it on one side.
        let mut before_with_null = before.clone();
        before_with_null[1]
            .as_object_mut()
            .unwrap()
            .insert("completion_note".to_string(), Value::Null);
        let connection = open(&graph).unwrap();
        let written = write_changed(&connection, &before_with_null, &after, true).unwrap();
        assert_eq!(
            written.present_ids.len(),
            1,
            "only the real change writes: {:?}",
            written.present_ids
        );
        drop(connection);
        drop(dir);
    }

    #[test]
    fn readback_batches_more_ids_than_sqlite_bind_limit() {
        let dir = TempDir::new().unwrap();
        let graph = two_node_graph(&dir);
        let connection = open(&graph).unwrap();
        let after: Vec<Value> = (0..1001)
            .map(|i| serde_json::json!({"id": format!("id-{i}")}))
            .collect();

        let error = confirm_ids_landed(&connection, &after).unwrap_err();

        assert!(error.contains("id-0"));
    }

    #[test]
    fn flipgate_child_extras_note_reads_key_survives_the_roundtrip() {
        // AC4-HP: a progress note carrying an unknown key keeps it through
        // save + export, so the two legs agree.
        let dir = TempDir::new().unwrap();
        let graph = two_node_graph(&dir);
        let mut rows = raw_rows(&graph);
        rows[0]["progress_notes"] =
            serde_json::json!([{"ts": "2026-09-14T00:00:00+00:00", "text": "note", "reads": [42]}]);
        std::fs::write(&graph, crate::graph_store::serialize_graph_file(&rows)).unwrap();
        authoritative_sync(&graph, &[], &rows).unwrap();
        let reloaded = read_entries(&graph).unwrap();
        let notes = reloaded[0]
            .get("progress_notes")
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(notes[0]["reads"], serde_json::json!([42]));
    }

    #[test]
    fn flipgate_child_extras_migration_adds_column_and_keeps_rows() {
        // AC5-EDGE: a db whose comments table lost the extras column is
        // migrated back on the next open, and legacy rows load with empty
        // extras.
        let (dir, graph) = fixture("graph.json");
        open(&graph).unwrap();
        let db = database_path(&graph);
        let connection = Connection::open(&db).unwrap();
        connection
            .execute_batch("ALTER TABLE comments DROP COLUMN extras;")
            .unwrap();
        drop(connection);
        let connection = open(&graph).unwrap();
        let has: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('comments') WHERE name = 'extras'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(has, 1, "the open re-added the extras column");
        let comments = crate::backlog::comments::load(&connection, "ab-one").unwrap();
        assert!(comments.is_empty(), "imported rows load fine: {comments:?}");
        drop(connection);
        drop(dir);
    }

    /// Seed a schema-2 store whose db rows lag the json: the title change
    /// after the seed is published to graph.json only, then the store is
    /// downgraded and one stale soak key is planted.
    fn schema2_graph_with_stale_rows(dir: &TempDir) -> PathBuf {
        let graph = two_node_graph(dir);
        let rows = raw_rows(&graph);
        authoritative_sync(&graph, &[], &rows).unwrap();
        let mut after = rows.clone();
        after[0]["title"] = Value::String("One renamed".into());
        std::fs::write(&graph, crate::graph_store::serialize_graph_file(&after)).unwrap();
        let connection = open(&graph).unwrap();
        connection
            .execute(
                "UPDATE graph_meta SET value = '2' WHERE key = 'schema_version'",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO graph_meta(key, value) VALUES('soak_clean_since_ms', '123')
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [],
            )
            .unwrap();
        drop(connection);
        graph
    }

    #[test]
    fn flipgate_schema_v3_sqlite_backend_stamps_without_rewrite() {
        // AC7-EDGE: under the sqlite backend graph.json is not
        // authoritative; the stamp moves and no row is rebuilt.
        let dir = TempDir::new().unwrap();
        let graph = schema2_graph_with_stale_rows(&dir);
        // Stamp the backend directly: set_backend opens the store, and an
        // open here would run the json-backend rebuild first.
        let connection = Connection::open(database_path(&graph)).unwrap();
        connection
            .execute(
                "INSERT INTO graph_meta(key, value) VALUES('backend', 'sqlite')
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [],
            )
            .unwrap();
        drop(connection);
        let entries = read_entries(&graph).unwrap();
        assert_eq!(
            entries[0]["title"], "One",
            "the stale row was NOT rewritten from json"
        );
        let connection = open(&graph).unwrap();
        let schema: String = connection
            .query_row(
                "SELECT value FROM graph_meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(schema, SCHEMA_VERSION);
    }

    // -- single-row mutations --------------------------------------------

    /// Pins both state roots the emit path resolves, so a test's gate event
    /// lands in the redirected space journal and never the operator's home.
    fn declare_test_roots(spaces: &std::path::Path) {
        std::env::set_var("FNO_SPACES_DIR", spaces);
        std::env::set_var(crate::paths::HOME_ENV, spaces.join("agents-home"));
    }

    fn seeded_sqlite_fixture() -> (TempDir, PathBuf) {
        let (dir, graph) = fixture("graph.json");
        std::fs::write(
            &graph,
            r#"{"entries": [
                {"id": "ab-one", "slug": "ab-one", "title": "One", "type": "feature",
                 "status": "idea", "priority": "p2", "domain": "code"},
                {"id": "ab-two", "slug": "ab-two", "title": "Two", "type": "feature",
                 "status": "ready", "priority": "p2", "domain": "code"}
            ]}"#,
        )
        .unwrap();
        open(&graph).unwrap();
        set_backend(&graph).unwrap();
        (dir, graph)
    }

    #[test]
    fn single_row_mutation_writes_only_target_rows_and_names_the_mutation() {
        let _env_lock = crate::claims::test_env_lock().lock().unwrap();
        let spaces = tempfile::TempDir::new().unwrap();
        declare_test_roots(spaces.path());
        let (_dir, graph) = seeded_sqlite_fixture();
        let before = export_rows(&open(&graph).unwrap()).unwrap();
        let ok = mutate_single_row(&graph, "comment_create", |rows| {
            for row in rows.iter_mut() {
                if entry_id_from(row) == Some("ab-one") {
                    row.as_object_mut()
                        .unwrap()
                        .insert("progress_notes".into(), serde_json::json!([{"body": "hi"}]));
                }
            }
            Ok(true)
        })
        .unwrap();
        assert!(ok);
        let after = export_rows(&open(&graph).unwrap()).unwrap();
        for (was, now) in before.iter().zip(after.iter()) {
            if entry_id_from(now) != Some("ab-one") {
                assert_eq!(
                    crate::graph_store::to_python_json(was),
                    crate::graph_store::to_python_json(now),
                    "a non-target row moved"
                );
            }
        }
        let target = after
            .iter()
            .find(|row| entry_id_from(row) == Some("ab-one"))
            .unwrap();
        assert!(target.get("progress_notes").is_some(), "target row moved");
        // Positive marker: the gate event names the mutation in the space journal.
        let journal = crate::paths::events_path(&std::env::current_dir().unwrap());
        let text = std::fs::read_to_string(journal).unwrap();
        assert!(text.contains("graph_write_gate") && text.contains("comment_create"));
    }

    #[test]
    fn single_row_concurrent_writes_both_persist_with_positive_readback() {
        let _env_lock = crate::claims::test_env_lock().lock().unwrap();
        let spaces = tempfile::TempDir::new().unwrap();
        declare_test_roots(spaces.path());
        let (_dir, graph) = seeded_sqlite_fixture();
        let graph_for_thread = graph.clone();
        let slow = std::thread::spawn(move || {
            mutate_single_row(&graph_for_thread, "comment_create", |rows| {
                // Hold the write transaction open so the other writer must
                // wait on BEGIN IMMEDIATE, landing after this commit.
                std::thread::sleep(std::time::Duration::from_millis(300));
                for row in rows.iter_mut() {
                    if entry_id_from(row) == Some("ab-one") {
                        row.as_object_mut().unwrap().insert(
                            "progress_notes".into(),
                            serde_json::json!([{"body": "from-slow"}]),
                        );
                    }
                }
                Ok(true)
            })
            .unwrap()
        });
        let ok_fast = mutate_single_row(&graph, "node_update", |rows| {
            for row in rows.iter_mut() {
                if entry_id_from(row) == Some("ab-two") {
                    row.as_object_mut()
                        .unwrap()
                        .insert("title".into(), serde_json::json!("Renamed"));
                }
            }
            Ok(true)
        })
        .unwrap();
        assert!(ok_fast);
        assert!(slow.join().unwrap());
        let rows = export_rows(&open(&graph).unwrap()).unwrap();
        // Positive readback: each concurrent write is read back BY VALUE.
        let one = rows
            .iter()
            .find(|r| entry_id_from(r) == Some("ab-one"))
            .unwrap();
        let two = rows
            .iter()
            .find(|r| entry_id_from(r) == Some("ab-two"))
            .unwrap();
        assert_eq!(
            one.get("progress_notes").unwrap()[0].get("body").unwrap(),
            "from-slow"
        );
        assert_eq!(two.get("title").unwrap(), "Renamed");
    }

    #[test]
    fn single_row_write_uses_current_db_state_not_stale_json() {
        let _env_lock = crate::claims::test_env_lock().lock().unwrap();
        let spaces = tempfile::TempDir::new().unwrap();
        declare_test_roots(spaces.path());
        let (_dir, graph) = seeded_sqlite_fixture();
        // A post-flip node lands in the db only; the stale JSON never learns
        // of it. The single-row path must still see and mutate it.
        mutate_single_row(&graph, "node_create", |rows| {
            rows.push(serde_json::json!({
                "id": "ab-flip", "slug": "ab-flip", "title": "Flip", "type": "feature",
                "status": "idea", "priority": "p2", "domain": "code"
            }));
            Ok(true)
        })
        .unwrap();
        let stale = std::fs::read_to_string(&graph).unwrap();
        assert!(!stale.contains("ab-flip"), "json leg moved under sqlite");
        let ok = mutate_single_row(&graph, "comment_create", |rows| {
            assert!(
                rows.iter().any(|row| entry_id_from(row) == Some("ab-flip")),
                "the authoritative read missed the db-only node"
            );
            Ok(true)
        })
        .unwrap();
        assert!(ok);
    }

    fn entry_id_from(row: &Value) -> Option<&str> {
        row.get("id").and_then(Value::as_str)
    }
}
