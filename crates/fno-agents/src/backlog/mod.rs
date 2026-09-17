//! The backlog store: graph.db lifecycle (schema, import, sync, export),
//! the backend naming, and (wave 5) parity. Each aggregate's tables live in
//! its owning module and no other file writes them (ruling 4);
//! `TABLE_OWNERS` names the owners and the table_ownership test enforces
//! it. JSON stays authoritative through this group: graph.db is the
//! relational shadow, and every mutation writes only the changed nodes'
//! rows in one transaction.

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
pub mod patch;
pub mod pull_requests;
pub mod receipt;
pub mod relations;
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

pub fn set_backend(graph: &Path, backend: Backend) -> Result<(), String> {
    let connection = open(graph)?;
    connection
        .execute(
            "INSERT INTO graph_meta(key, value) VALUES('backend', ?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![backend.name()],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
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
    import_if_needed(&mut connection, graph)?;
    decisions::ensure_table(&connection)?;
    decisions::import_if_needed(&mut connection, graph)?;
    Ok(connection)
}

/// The one-shot import: a db holding the blob `entries` table and no
/// `nodes` imports its rows into the owned tables, drops `entries`, and
/// stamps the current schema_version. A path with graph.json and no
/// graph.db imports on first open, which keeps the hundreds of test files
/// that seed graph.json fixtures working. A populated store re-imports
/// never; it may rebuild once (see [`rebuild_if_schema_v2`]).
fn import_if_needed(connection: &mut Connection, graph: &Path) -> Result<(), String> {
    let nodes_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0))
        .map_err(|error| error.to_string())?;
    if nodes_count > 0 {
        return rebuild_if_schema_v2(connection, graph);
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
        return Ok(());
    }
    if rows.is_empty() && !has_entries {
        // An empty-or-absent graph with no blob: stamp nothing; the first
        // shadow write stamps the meta.
        if graph.exists() {
            stamp_meta(connection, "schema_version", SCHEMA_VERSION)?;
        }
        return Ok(());
    }
    let transaction = connection
        .transaction()
        .map_err(|error| error.to_string())?;
    for (ordinal, row) in rows.iter().enumerate() {
        // A row the model cannot represent (a minimal legacy fixture row with
        // no slug/status) is skipped, not fatal: JSON stays authoritative,
        // the shadow is best-effort, and parity surfaces the gap honestly.
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

/// Schema 3: a populated schema-2 store under the json backend rebuilds
/// its rows once from authoritative graph.json. The raw-baseline shadow
/// diff and the child-extras columns stop NEW drift; this rebuild visits
/// the rows that already drifted while normalization-only changes were
/// being skipped. It is also the soak restart: the rebuild deletes the
/// graph_meta soak keys, so the next clean sample starts a fresh 7-day
/// clock. Under the sqlite backend graph.json is not authoritative, so
/// the stamp moves and nothing is rewritten. No live store runs sqlite
/// today.
fn rebuild_if_schema_v2(connection: &mut Connection, graph: &Path) -> Result<(), String> {
    let target: i64 = SCHEMA_VERSION.parse().unwrap_or(i64::MAX);
    let current: i64 = meta(connection, "schema_version")?
        .and_then(|raw| raw.parse::<i64>().ok())
        .unwrap_or(2);
    if current >= target {
        return Ok(());
    }
    if backend(graph) == Backend::Sqlite {
        return stamp_meta(connection, "schema_version", SCHEMA_VERSION);
    }
    // The rebuild reads graph.json; a file that does not parse, or that
    // carries no entries ARRAY, is left for the next open, and parity
    // surfaces the gap loudly meanwhile. An entries-less json is refused
    // by parity too: it is malformed for this store, never an empty
    // authority, so it must not read as "delete every db row".
    let doc = std::fs::read_to_string(graph)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
    let has_entries_array = doc
        .as_ref()
        .map(|doc| doc.get("entries").map(Value::is_array).unwrap_or(false))
        .unwrap_or(false);
    if !has_entries_array {
        return Ok(());
    }
    let rows: Vec<Value> = doc
        .expect("checked above")
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let transaction = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    // Re-read under the write lock: a concurrent opener may have rebuilt
    // already, and two rebuilds must not interleave.
    let raced: i64 = meta(&transaction, "schema_version")?
        .and_then(|raw| raw.parse::<i64>().ok())
        .unwrap_or(2);
    if raced >= target {
        return Ok(());
    }
    let ids: Vec<String> = {
        let mut statement = transaction
            .prepare("SELECT id FROM nodes")
            .map_err(|error| error.to_string())?;
        let mapped = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| error.to_string())?;
        mapped.collect::<Result<Vec<_>, _>>()
    }
    .map_err(|error: rusqlite::Error| error.to_string())?;
    for id in &ids {
        delete_aggregate(&transaction, id)?;
    }
    for (ordinal, row) in rows.iter().enumerate() {
        // Same best-effort rule as the import: a row the model cannot
        // represent is skipped so the JSON publish never inherits a
        // rebuild failure.
        let Ok(mut node) = Node::from_json(row) else {
            continue;
        };
        node.ordinal = ordinal as i64;
        save_aggregate(&transaction, &node).map_err(|error| format!("rebuild: {error}"))?;
    }
    transaction
        .execute(
            "DELETE FROM graph_meta WHERE key IN ('soak_clean_since_ms', 'soak_clean_days',
             'soak_last_sample_ms', 'soak_last_divergent')",
            [],
        )
        .map_err(|error| error.to_string())?;
    stamp_version_fields(&transaction, &content_version(&rows))?;
    stamp_meta(&transaction, "schema_version", SCHEMA_VERSION)?;
    transaction.commit().map_err(|error| error.to_string())
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

/// The relational shadow write: only the ids whose canonical JSON differs
/// between `before` and `after` are written, each through its owning
/// module, in one transaction. A node absent from `after` is deleted with
/// its child rows. The db holds no version counters beyond graph_meta.
pub fn shadow_sync(
    graph: &Path,
    before: &[Value],
    after: &[Value],
    json_version: &str,
) -> Result<PathBuf, String> {
    let mut connection = open(graph)?;
    let transaction = connection
        .transaction()
        .map_err(|error| error.to_string())?;
    let _report = write_changed(&transaction, before, after, false)?;
    stamp_version(&transaction, json_version)?;
    transaction.commit().map_err(|error| error.to_string())?;
    Ok(database_path(graph))
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
    let report = write_changed(&transaction, before, after, true)?;
    let version = content_version(after);
    stamp_version(&transaction, &version)?;
    transaction.commit().map_err(|error| error.to_string())?;
    confirm_ids_landed(&connection, &report)?;
    Ok(version)
}

struct WriteReport {
    present_ids: Vec<String>,
    deleted_ids: Vec<String>,
}

fn confirm_ids_landed(connection: &Connection, report: &WriteReport) -> Result<(), String> {
    let ids: Vec<&str> = report
        .present_ids
        .iter()
        .chain(report.deleted_ids.iter())
        .map(String::as_str)
        .collect();
    if ids.is_empty() {
        return Ok(());
    }

    let placeholders = std::iter::repeat_n("?", ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let query = format!("SELECT id FROM nodes WHERE id IN ({placeholders})");
    let mut statement = connection
        .prepare(&query)
        .map_err(|error| error.to_string())?;
    let stored: std::collections::BTreeSet<String> = statement
        .query_map(rusqlite::params_from_iter(ids.iter().copied()), |row| {
            row.get::<_, String>(0)
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|error| error.to_string())?;
    let missing: Vec<&str> = report
        .present_ids
        .iter()
        .map(String::as_str)
        .filter(|id| !stored.contains(*id))
        .collect();
    let extra: Vec<&str> = report
        .deleted_ids
        .iter()
        .map(String::as_str)
        .filter(|id| stored.contains(*id))
        .collect();
    if missing.is_empty() && extra.is_empty() {
        return Ok(());
    }
    let missing: Vec<&str> = expected.difference(&stored).map(String::as_str).collect();
    let extra: Vec<&str> = stored.difference(&expected).map(String::as_str).collect();
    Err(format!(
        "publish read-back mismatch: missing ids {missing:?}; extra ids {extra:?}"
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
/// Returns the ids acted on, split by rows that should be present or deleted.
pub(crate) fn write_changed(
    connection: &Connection,
    before: &[Value],
    after: &[Value],
    strict: bool,
) -> Result<WriteReport, String> {
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
                let mut node = match Node::from_json(body) {
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
/// the relational export the parity compare reads.
pub fn read_entries(graph: &Path) -> Result<Vec<Value>, String> {
    let connection = open(graph)?;
    export_rows(&connection)
}

/// The rows behind an open connection, in ordinal order.
pub fn export_rows(connection: &Connection) -> Result<Vec<Value>, String> {
    if meta(connection, "version")?.is_none() {
        return Err("SQLite graph has no version".into());
    }
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

/// Flip `graph_meta.backend` and stamp `backend_since_ms` on an actual
/// change. The read, the write, and the stamp run in one IMMEDIATE
/// transaction, so two concurrent flips serialize: the second sees the
/// first's backend and keeps its since stamp. A re-run of the same backend
/// keeps the original stamp: the clock the `--status` days count reads
/// must not reset under an idempotent verb.
pub fn flip_backend(graph: &Path, target: Backend) -> Result<(Backend, Option<u128>), String> {
    let mut connection = open(graph)?;
    let transaction = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let previous = match meta(&transaction, "backend")? {
        Some(value) if value == Backend::Sqlite.name() => Backend::Sqlite,
        _ => Backend::Json,
    };
    stamp_meta(&transaction, "backend", target.name())?;
    let since = if previous != target {
        let stamp = now_ms();
        stamp_meta(&transaction, "backend_since_ms", &stamp.to_string())?;
        Some(stamp)
    } else {
        meta(&transaction, "backend_since_ms")?.and_then(|value| value.parse::<u128>().ok())
    };
    transaction.commit().map_err(|error| error.to_string())?;
    Ok((previous, since))
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

/// Record one parity sample into graph_meta, the soak evidence the flip
/// gate reads. One transaction per sample: a clean sample extends the
/// current run (start the clock when absent, add today's UTC date), a
/// divergent sample ends it (drop the clock, name the ids).
pub fn record_parity_sample(
    graph: &Path,
    report: &ParityReport,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(), String> {
    let mut connection = open(graph)?;
    let transaction = connection
        .transaction()
        .map_err(|error| error.to_string())?;
    stamp_meta(
        &transaction,
        "soak_last_sample_ms",
        &now.timestamp_millis().to_string(),
    )?;
    if report.divergent > 0 {
        let divergent = serde_json::json!({
            "ts": now.to_rfc3339(),
            "divergent": report.divergent,
            "ids": report.divergent_ids,
        });
        stamp_meta(&transaction, "soak_last_divergent", &divergent.to_string())?;
        transaction
            .execute(
                "DELETE FROM graph_meta WHERE key IN ('soak_clean_since_ms', 'soak_clean_days')",
                [],
            )
            .map_err(|error| error.to_string())?;
    } else {
        if meta(&transaction, "soak_clean_since_ms")?.is_none() {
            stamp_meta(
                &transaction,
                "soak_clean_since_ms",
                &now.timestamp_millis().to_string(),
            )?;
        }
        let mut days: Vec<String> = meta(&transaction, "soak_clean_days")?
            .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
            .unwrap_or_default();
        let today = now.date_naive().to_string();
        if !days.contains(&today) {
            days.push(today);
            let days = serde_json::to_string(&days).map_err(|error| error.to_string())?;
            stamp_meta(&transaction, "soak_clean_days", &days)?;
        }
    }
    transaction.commit().map_err(|error| error.to_string())
}

/// Whether a parity sample was already recorded on today's UTC date: the
/// sampler's once-per-day floor, so a quiet store still accrues clean days.
pub fn soak_sampled_today(graph: &Path) -> bool {
    let Ok(connection) = open(graph) else {
        return false;
    };
    let Some(ms) = meta(&connection, "soak_last_sample_ms")
        .ok()
        .flatten()
        .and_then(|raw| raw.parse::<i64>().ok())
    else {
        return false;
    };
    match chrono::DateTime::from_timestamp_millis(ms) {
        Some(ts) => ts.date_naive() == chrono::Utc::now().date_naive(),
        None => false,
    }
}

/// The soak evidence the flip needs, read from the graph_meta keys
/// `record_parity_sample` maintains: one gap line per failed requirement,
/// empty when the soak is clean. Old samples cannot block a later clean
/// run, because a divergent sample deletes the run it ended.
pub fn soak_gaps(graph: &Path, now: chrono::DateTime<chrono::Utc>) -> Vec<String> {
    let connection = match open(graph) {
        Ok(connection) => connection,
        Err(_) => return vec!["no clean parity sample recorded yet".into()],
    };
    let since_ms = meta(&connection, "soak_clean_since_ms")
        .ok()
        .flatten()
        .and_then(|raw| raw.parse::<i64>().ok());
    let since = since_ms.and_then(chrono::DateTime::from_timestamp_millis);
    let Some(since) = since else {
        // No live run. A recorded divergent sample names itself; with no
        // evidence at all the soak has simply never run.
        let last_divergent = meta(&connection, "soak_last_divergent")
            .ok()
            .flatten()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());
        if let Some(value) = last_divergent {
            let ts = value
                .get("ts")
                .and_then(Value::as_str)
                .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
                .map(chrono::DateTime::<chrono::Utc>::from);
            let divergent = value.get("divergent").and_then(Value::as_i64).unwrap_or(0);
            let ids = value
                .get("ids")
                .and_then(Value::as_array)
                .map(|ids| {
                    ids.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            if let Some(ts) = ts {
                return vec![format!(
                    "last sample {} had {divergent} divergent row(s): {ids}; the soak restarts at the next clean sample",
                    ts.format("%Y-%m-%dT%H:%M:%SZ")
                )];
            }
        }
        return vec!["no clean parity sample recorded yet".into()];
    };
    let mut gaps = Vec::new();
    let covered: std::collections::HashSet<String> = meta(&connection, "soak_clean_days")
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
        .map(|list| list.into_iter().collect())
        .unwrap_or_default();
    let mut missing = Vec::new();
    let mut day = since.date_naive();
    while day <= now.date_naive() {
        let text = day.to_string();
        if !covered.contains(&text) {
            missing.push(text);
        }
        day = day.succ_opt().unwrap_or(day);
        if missing.len() > 64 {
            break;
        }
    }
    if !missing.is_empty() {
        gaps.push(format!(
            "no sample on {} day(s): {}",
            missing.len(),
            missing.join(", ")
        ));
    }
    gaps
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

/// One parity sample: authoritative JSON vs the relational export.
#[derive(Clone, Debug)]
pub struct ParityReport {
    pub rows: usize,
    pub divergent: usize,
    /// The first ten divergent ids; enough to name the drift, bounded so a
    /// badly drifted graph cannot flood the journal.
    pub divergent_ids: Vec<String>,
}

/// Compare graph.json against the relational export, the only parity
/// implementation (the Python leg is a thin client over the keeper op that
/// serves this). Both sides canonicalize through the null-stripped
/// to_python_json form, so an explicit null and an absent key agree. The
/// db must already exist: parity never creates a store to compare against.
pub fn parity(graph: &Path) -> Result<ParityReport, String> {
    if !database_path(graph).exists() {
        return Err(format!(
            "no relational store at {} to compare against",
            database_path(graph).display()
        ));
    }
    let text = std::fs::read_to_string(graph).map_err(|error| error.to_string())?;
    let doc: Value = serde_json::from_str(&text)
        .map_err(|error| format!("{} is invalid JSON: {error}", graph.display()))?;
    let json_rows = canonical_rows(
        doc.get("entries")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("{} has no entries array", graph.display()))?,
        "graph.json",
    )?;
    let connection = open(graph)?;
    let export = export_rows(&connection)?;
    let db_rows = canonical_rows(&export, "relational export")?;
    let mut divergent_ids = Vec::new();
    let mut keys: Vec<String> = json_rows.keys().chain(db_rows.keys()).cloned().collect();
    keys.sort();
    keys.dedup();
    for key in &keys {
        if json_rows.get(key) != db_rows.get(key) && divergent_ids.len() < 10 {
            divergent_ids.push(key.clone());
        }
    }
    let divergent = keys
        .iter()
        .filter(|key| json_rows.get(*key) != db_rows.get(*key))
        .count();
    Ok(ParityReport {
        rows: json_rows.len().max(db_rows.len()),
        divergent,
        divergent_ids,
    })
}

/// Recursively sort object keys, the parity canonical form: the JSON leg's
/// key order and the export's key order differ, and only the sorted form
/// compares equal.
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
/// parity equality rule both `canonical_rows` and `write_changed` share.
fn canonical_row(row: &Value) -> String {
    crate::graph_store::to_python_json(&sorted_value(&crate::backlog::model::strip_nulls_value(
        row,
    )))
}

/// id -> canonical JSON for a row list, refusing duplicate ids loudly.
fn canonical_rows(
    rows: &[Value],
    label: &str,
) -> Result<std::collections::BTreeMap<String, String>, String> {
    let mut out = std::collections::BTreeMap::new();
    for row in rows {
        let Some(id) = crate::graph_store::entry_id(row) else {
            return Err(format!("{label} holds a row without an id"));
        };
        if out.contains_key(id) {
            return Err(format!("duplicate id in {label}: {id}"));
        }
        out.insert(id.to_string(), canonical_row(row));
    }
    Ok(out)
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
    fn backend_reads_json_when_db_absent() {
        let (_dir, graph) = fixture("graph.json");
        assert_eq!(backend(&graph), Backend::Json);
        assert!(!database_path(&graph).exists());
    }

    fn sample_report(divergent: usize, ids: Vec<String>) -> ParityReport {
        ParityReport {
            rows: 2,
            divergent,
            divergent_ids: ids,
        }
    }

    #[test]
    fn flipgate_soak_days_covered_read_clean() {
        // AC9-HP: clean samples covering every UTC day of the run read
        // clean, whatever the run's age. Law d-bbbb5a26 waived the 7-day
        // clock, so a same-day run passes with today's sample alone.
        let (dir, graph) = fixture("graph.json");
        let now = chrono::Utc::now();
        for offset in (0..8).rev() {
            record_parity_sample(
                &graph,
                &sample_report(0, vec![]),
                now - chrono::Duration::days(offset),
            )
            .unwrap();
        }
        assert_eq!(soak_gaps(&graph, now), Vec::<String>::new());
        let (day_dir, day_graph) = fixture("graph.json");
        record_parity_sample(&day_graph, &sample_report(0, vec![]), now).unwrap();
        assert_eq!(soak_gaps(&day_graph, now), Vec::<String>::new());
        drop(day_dir);
        drop(dir);
    }

    #[test]
    fn flipgate_soak_divergent_sample_ends_the_run_and_names_its_ids() {
        // AC10-EDGE: a three-day clean run ends at one divergent sample;
        // the gap names the ids, and the next clean sample restarts the
        // clock at its own instant.
        let (dir, graph) = fixture("graph.json");
        let now = chrono::Utc::now();
        for offset in (1..4).rev() {
            record_parity_sample(
                &graph,
                &sample_report(0, vec![]),
                now - chrono::Duration::days(offset),
            )
            .unwrap();
        }
        record_parity_sample(&graph, &sample_report(1, vec!["x-bad".into()]), now).unwrap();
        let gaps = soak_gaps(&graph, now);
        assert!(
            gaps.iter()
                .any(|gap| gap.contains("x-bad") && gap.contains("restarts")),
            "{gaps:?}"
        );
        let clean = sample_report(0, vec![]);
        let restart = now + chrono::Duration::hours(1);
        record_parity_sample(&graph, &clean, restart).unwrap();
        let connection = open(&graph).unwrap();
        let since = meta(&connection, "soak_clean_since_ms").unwrap();
        assert_eq!(since, Some(restart.timestamp_millis().to_string()));
        drop(connection);
        drop(dir);
    }

    #[test]
    fn flipgate_soak_missing_day_names_the_gap() {
        // AC11-EDGE: a run whose day 2 has no sample names that date, and
        // a young run names no age gap (law d-bbbb5a26 waived the 7-day
        // clock).
        let (dir, graph) = fixture("graph.json");
        let now = chrono::Utc::now();
        let clean = sample_report(0, vec![]);
        record_parity_sample(&graph, &clean, now - chrono::Duration::days(2)).unwrap();
        record_parity_sample(&graph, &clean, now).unwrap();
        let gaps = soak_gaps(&graph, now);
        let skipped = (now - chrono::Duration::days(1)).date_naive().to_string();
        assert!(
            gaps.iter()
                .any(|gap| gap.contains("no sample on 1 day(s)") && gap.contains(&skipped)),
            "{gaps:?}"
        );
        assert!(
            !gaps.iter().any(|gap| gap.contains("day(s) old")),
            "{gaps:?}"
        );
        drop(dir);
    }

    #[test]
    fn flipgate_soak_no_evidence_names_the_gap() {
        let (dir, graph) = fixture("graph.json");
        let gaps = soak_gaps(&graph, chrono::Utc::now());
        assert_eq!(
            gaps,
            vec!["no clean parity sample recorded yet".to_string()]
        );
        drop(dir);
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
    fn flip_backend_stamps_since_only_on_change() {
        let (dir, graph) = fixture("graph.json");
        // The probe is read-only: an absent db reads None and stays absent.
        assert_eq!(backend_since(&graph).unwrap(), None);
        assert!(!database_path(&graph).exists());
        let (previous, since1) = flip_backend(&graph, Backend::Sqlite).unwrap();
        assert_eq!(previous, Backend::Json);
        let since1 = since1.expect("a real flip stamps since");
        // An idempotent re-run keeps the original clock.
        let (previous, since2) = flip_backend(&graph, Backend::Sqlite).unwrap();
        assert_eq!(previous, Backend::Sqlite);
        assert_eq!(since2, Some(since1));
        // Rolling back stamps a NEW since.
        let (_, since3) = flip_backend(&graph, Backend::Json).unwrap();
        let since3 = since3.expect("a real flip stamps since");
        assert!(since3 > since1, "rollback moves the clock forward");
        drop(dir);
    }

    #[test]
    fn backend_reads_graph_meta_backend() {
        let (dir, graph) = fixture("graph.json");
        set_backend(&graph, Backend::Sqlite).unwrap();
        assert_eq!(backend(&graph), Backend::Sqlite);
        // An unset key keeps the rollback default.
        let connection = open(&graph).unwrap();
        connection
            .execute("DELETE FROM graph_meta WHERE key = 'backend'", [])
            .unwrap();
        drop(connection);
        assert_eq!(backend(&graph), Backend::Json);
        drop(dir);
    }

    #[test]
    fn backend_change_is_visible_without_restart() {
        // AC5-EDGE: a second process flips the backend; the next read on a
        // pre-existing connection sees it.
        let (dir, graph) = fixture("graph.json");
        let connection = open(&graph).unwrap();
        assert_eq!(backend(&graph), Backend::Json);
        set_backend(&graph, Backend::Sqlite).unwrap();
        drop(connection);
        assert_eq!(backend(&graph), Backend::Sqlite);
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
        shadow_sync(&graph, &before, &after, "sha256:test").unwrap();

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
    fn parity_clean_copies_compare_clean_and_a_mutated_row_diverges() {
        // AC2-HP's mechanical core: clean compares clean, one changed
        // title diverges naming that id.
        let dir = TempDir::new().unwrap();
        let graph = two_node_graph(&dir);
        let rows = read_entries(&graph).unwrap();
        // Seed the relational side from the same rows.
        shadow_sync(&graph, &[], &rows, "sha256:seed").unwrap();
        let report = parity(&graph).unwrap();
        assert_eq!(report.rows, 2);
        assert_eq!(report.divergent, 0, "clean copies compare clean");
        // Mutate one copied row's title on the JSON side.
        let mut doc: Value =
            serde_json::from_str(&std::fs::read_to_string(&graph).unwrap()).unwrap();
        doc["entries"][0]["title"] = Value::String("One mutated".into());
        std::fs::write(&graph, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
        let report = parity(&graph).unwrap();
        assert_eq!(report.divergent, 1, "the mutated row diverges");
        assert_eq!(report.divergent_ids, vec!["ab-one".to_string()]);
    }

    #[test]
    fn parity_refuses_a_missing_db_and_duplicate_ids() {
        let dir = TempDir::new().unwrap();
        let graph = two_node_graph(&dir);
        let err = parity(&graph).unwrap_err();
        assert!(
            err.contains("no relational store"),
            "parity never creates a store to compare against: {err}"
        );
        // Seed the store, then duplicate an id in the JSON.
        let rows = read_entries(&graph).unwrap();
        shadow_sync(&graph, &[], &rows, "sha256:seed").unwrap();
        let mut doc: Value =
            serde_json::from_str(&std::fs::read_to_string(&graph).unwrap()).unwrap();
        let dup = doc["entries"][1].clone();
        doc["entries"].as_array_mut().unwrap().push(dup);
        std::fs::write(&graph, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
        let err = parity(&graph).unwrap_err();
        assert!(
            err.contains("duplicate id"),
            "a duplicate id is refused loudly: {err}"
        );
    }

    #[test]
    fn backlog_schema_delete_removes_the_whole_aggregate() {
        let dir = TempDir::new().unwrap();
        let graph = two_node_graph(&dir);
        let before = read_entries(&graph).unwrap();
        let after: Vec<Value> = before[1..].to_vec();
        shadow_sync(&graph, &before, &after, "sha256:test").unwrap();
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
    fn flipgate_shadow_normalization_only_change_reaches_the_store() {
        // AC1-HP: the file row lacks the default lists; a mutation on a
        // DIFFERENT node publishes the defaulted form. The diff must see
        // that change against the raw baseline and save the row.
        let dir = TempDir::new().unwrap();
        let graph = two_node_graph(&dir);
        let raw = raw_rows(&graph);
        // Seed the store from the raw file: the db now holds ab-one with no
        // tags key, exactly what the last publish wrote.
        shadow_sync(&graph, &[], &raw, "sha256:seed").unwrap();
        let mut after = raw.clone();
        // The Python mutator sends defaulted rows: ab-one gains "tags": [].
        after[0]
            .as_object_mut()
            .unwrap()
            .insert("tags".to_string(), Value::Array(vec![]));
        // A change on the other node is what triggers the publish.
        after[1]
            .as_object_mut()
            .unwrap()
            .insert("title".to_string(), Value::String("Two changed".into()));
        std::fs::write(&graph, crate::graph_store::serialize_graph_file(&after)).unwrap();
        let outcome = crate::graph_store::locked_mutate_with_hook(
            &graph,
            crate::graph_store::MutateInput {
                entries: after.clone(),
                canonical_path: None,
                base_version: crate::graph_store::file_content_version(&graph),
                plan_rungs: None,
            },
            std::time::Duration::from_secs(10),
            None,
        )
        .unwrap();
        assert!(
            outcome.shadow_warning.is_none(),
            "{:?}",
            outcome.shadow_warning
        );
        let report = parity(&graph).unwrap();
        assert_eq!(
            report.divergent, 0,
            "defaulted row reached the store: {report:?}"
        );
    }

    #[test]
    fn flipgate_shadow_superseded_settle_reaches_the_store() {
        // AC2-HP: the raw row is blocked with superseded_by set; the
        // mutation pipeline settles it to superseded. The settle must reach
        // the store, not only graph.json.
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
        std::fs::write(&graph, crate::graph_store::serialize_graph_file(&raw)).unwrap();
        shadow_sync(&graph, &[], &raw, "sha256:seed").unwrap();
        // Mutate the OTHER node; the pipeline settles ab-two itself.
        let mut after = raw_rows(&graph);
        after[0]
            .as_object_mut()
            .unwrap()
            .insert("title".to_string(), Value::String("One changed".into()));
        let outcome = crate::graph_store::locked_mutate_with_hook(
            &graph,
            crate::graph_store::MutateInput {
                entries: after.clone(),
                canonical_path: None,
                base_version: crate::graph_store::file_content_version(&graph),
                plan_rungs: None,
            },
            std::time::Duration::from_secs(10),
            None,
        )
        .unwrap();
        assert!(
            outcome.shadow_warning.is_none(),
            "{:?}",
            outcome.shadow_warning
        );
        let report = parity(&graph).unwrap();
        assert_eq!(
            report.divergent, 0,
            "the settle reached the store: {report:?}"
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
    fn an_unrepresentable_row_refuses_the_publish_instead_of_dropping_it() {
        let dir = TempDir::new().unwrap();
        let graph = two_node_graph(&dir);
        let before = raw_rows(&graph);
        shadow_sync(&graph, &[], &before, "sha256:seed").unwrap();
        let mut after = before.clone();
        after[0]["status"] = Value::String("not-a-status".into());

        let error = authoritative_sync(&graph, &before, &after).unwrap_err();

        assert!(error.contains("ab-one"), "error names the dropped row: {error}");
        assert!(error.contains("status"), "error names the parse failure: {error}");
    }

    #[test]
    fn shadow_sync_skips_an_unrepresentable_row_without_refusing_the_publish() {
        let dir = TempDir::new().unwrap();
        let graph = two_node_graph(&dir);
        let before = raw_rows(&graph);
        shadow_sync(&graph, &[], &before, "sha256:seed").unwrap();
        let mut after = before.clone();
        after[0]["status"] = Value::String("not-a-status".into());

        shadow_sync(&graph, &before, &after, "sha256:next").unwrap();

        let connection = open(&graph).unwrap();
        let version: String = connection
            .query_row(
                "SELECT value FROM graph_meta WHERE key = 'version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, "sha256:next");
        let status: String = connection
            .query_row("SELECT status FROM nodes WHERE id = 'ab-one'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(status, "idea");
    }

    #[test]
    fn a_landed_publish_reads_every_id_back() {
        let dir = TempDir::new().unwrap();
        let graph = two_node_graph(&dir);
        let before = raw_rows(&graph);
        shadow_sync(&graph, &[], &before, "sha256:seed").unwrap();
        let mut after = before.clone();
        after[0]["title"] = Value::String("One renamed".into());

        authoritative_sync(&graph, &before, &after).unwrap();

        let connection = open(&graph).unwrap();
        let mut statement = connection
            .prepare("SELECT id FROM nodes ORDER BY ordinal")
            .unwrap();
        let stored: Vec<String> = statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let expected: Vec<String> = after
            .iter()
            .map(|row| row["id"].as_str().unwrap().to_string())
            .collect();

        assert_eq!(stored, expected);
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
        shadow_sync(&graph, &[], &rows, "sha256:seed").unwrap();
        let reloaded = read_entries(&graph).unwrap();
        let notes = reloaded[0]
            .get("progress_notes")
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(notes[0]["reads"], serde_json::json!([42]));
        let report = parity(&graph).unwrap();
        assert_eq!(
            report.divergent, 0,
            "flipgate_child_extras_note_reads_key_survives_the_roundtrip: {report:?}"
        );
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
        shadow_sync(&graph, &[], &rows, "sha256:one").unwrap();
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
    fn flipgate_schema_v3_rebuild_clears_drift_and_soak() {
        // AC6-HP: the first open of a populated schema-2 store rebuilds
        // from graph.json, reads parity clean, and leaves no soak key.
        let dir = TempDir::new().unwrap();
        let graph = schema2_graph_with_stale_rows(&dir);
        let entries = read_entries(&graph).unwrap();
        assert_eq!(entries[0]["title"], "One renamed", "rows rebuilt from json");
        let report = parity(&graph).unwrap();
        assert_eq!(
            report.divergent, 0,
            "the rebuild cleared the drift: {report:?}"
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
        let soak: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM graph_meta WHERE key LIKE 'soak_%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(soak, 0, "the soak keys were deleted");
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

    #[test]
    fn flipgate_schema_v3_entriesless_json_never_wipes_the_db() {
        // A json that parses but holds no entries array is malformed for
        // this store, never an empty authority: the rebuild skips it and
        // the db rows stay.
        let dir = TempDir::new().unwrap();
        let graph = schema2_graph_with_stale_rows(&dir);
        std::fs::write(&graph, b"{\"entries\": null}").unwrap();
        let entries = read_entries(&graph).unwrap();
        assert_eq!(entries[0]["title"], "One", "the rows survived");
        let connection = open(&graph).unwrap();
        let schema: String = connection
            .query_row(
                "SELECT value FROM graph_meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(schema, "2", "no rebuild stamp on a malformed authority");
    }

    #[test]
    fn flipgate_schema_v3_concurrent_opens_both_reach_schema_3() {
        // AC8-EDGE: two openers racing the same schema-2 store both
        // succeed; one rebuilds, one observes the re-read stamp.
        let dir = TempDir::new().unwrap();
        let graph = schema2_graph_with_stale_rows(&dir);
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let path = graph.clone();
                std::thread::spawn(move || read_entries(&path).map(|rows| rows.len()))
            })
            .collect();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }
        let report = parity(&graph).unwrap();
        assert_eq!(
            report.divergent, 0,
            "parity clean after the race: {report:?}"
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
        set_backend(&graph, Backend::Sqlite).unwrap();
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
