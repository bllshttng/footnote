//! The backlog store: graph.db lifecycle (schema, import and sync). Each aggregate's tables live in
//! its owning module and no other file writes them (ruling 4);
//! `TABLE_OWNERS` names the owners and the table_ownership test enforces
//! Every mutation writes only the changed nodes' rows in one transaction.

pub mod api;
pub mod cli;
pub mod commands;
pub mod comments;
pub mod costs;
pub mod decisions;
pub mod done_evidence;
pub mod encounters;
pub mod entities;
pub mod epic_cap;
pub mod find_cli;
pub mod findings;
pub mod get_cli;
pub mod idea_cap;
pub mod model;
pub mod node_state;
pub mod nodes;
pub mod note_cli;
pub mod note_history;
pub mod note_migrate;
pub mod note_stale;
pub mod orphan_plans;
pub mod patch;
pub mod pull_requests;
pub mod receipt;
pub mod relations;
pub mod render;
pub mod schema_v4;
pub mod search;
pub mod sessions;
pub mod settings;

use crate::backlog::model::Node;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The schema stamp the import writes after the blob `entries` table is
/// dropped. Schema 4 (see schema_v4.rs) is the shape every table is born in.
pub const SCHEMA_VERSION: &str = "4";

/// Each aggregate's owning module (ruling 4). The table_ownership test
/// scans src/ against this map: a write to an owned table outside its
/// owner's file fails naming file and line.
pub const TABLE_OWNERS: &[(&str, &str)] = &[
    ("nodes", "backlog/nodes.rs"),
    ("node_claims", "backlog/nodes.rs"),
    ("node_dispatch", "backlog/nodes.rs"),
    ("node_provenance", "backlog/nodes.rs"),
    ("supersessions", "backlog/nodes.rs"),
    ("nodes_raw", "backlog/nodes.rs"),
    ("relations", "backlog/relations.rs"),
    ("relations_unresolved", "backlog/relations.rs"),
    ("nodes_fts", "backlog/search.rs"),
    ("comments", "backlog/comments.rs"),
    ("encounters", "backlog/encounters.rs"),
    ("pull_requests", "backlog/pull_requests.rs"),
    ("sessions", "backlog/sessions.rs"),
    ("decisions", "backlog/decisions.rs"),
    ("node_decisions", "backlog/decisions.rs"),
    ("node_costs", "backlog/costs.rs"),
    ("findings", "backlog/findings.rs"),
    ("harnesses", "backlog/entities.rs"),
    ("models", "backlog/entities.rs"),
    ("agent_sessions", "backlog/entities.rs"),
];

/// The store's key-value table, with the schema-4 stamps every table has.
pub(crate) fn graph_meta_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS graph_meta (
             key TEXT PRIMARY KEY,
             value TEXT NOT NULL{}
         );",
        schema_v4::stamps("graph_meta")
    )
}

/// Schema-4 migration copy of graph_meta from its `_v3` rename.
pub(crate) fn copy_graph_meta_from_v3(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch("INSERT INTO graph_meta (key, value) SELECT key, value FROM graph_meta_v3;")
        .map_err(|error| format!("schema v4 graph_meta copy: {error}"))
}

/// Every owning module's triggers: updated_at on each table, the entity
/// parents, and the relation park and promote pair. Created after the
/// tables, and after the schema-4 copies, which must not fire them.
pub(crate) fn ensure_triggers(connection: &Connection) -> Result<(), String> {
    let all = [
        schema_v4::touch("graph_meta"),
        entities::triggers(),
        nodes::triggers(),
        sessions::triggers(),
        comments::triggers(),
        encounters::triggers(),
        pull_requests::triggers(),
        relations::triggers(),
        decisions::triggers(),
        costs::triggers(),
        findings::triggers(),
    ]
    .concat();
    connection
        .execute_batch(&all)
        .map_err(|error| error.to_string())
}

pub(crate) fn now_ms() -> u128 {
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

/// The sole graph store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Sqlite,
}

impl Backend {
    pub fn name(self) -> &'static str {
        "sqlite"
    }
}

/// The graph store implementation.
pub fn backend(_graph: &Path) -> Backend {
    Backend::Sqlite
}

/// A connection for a WRITE path: identical to [`open`], kept as a distinct
/// name so the table-ownership scan can tell read-only opens from writes.
pub(crate) fn write_connection(graph: &Path) -> Result<Connection, String> {
    open(graph)
}

pub(crate) fn open(graph: &Path) -> Result<Connection, String> {
    let path = database_path(graph);
    crate::live_store_fence::refuse_worktree_build_on_operator_store(&path)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    // First contact serializes on the store lock: two processes creating the
    // db race the journal_mode pragma, and busy_timeout does not cover it.
    let _creation_lock = if path.exists() {
        None
    } else {
        Some(
            crate::graph_store::BoundedLock::acquire(graph, Duration::from_secs(10))
                .map_err(|error| error.to_string())?,
        )
    };
    open_connection(graph)
}

/// Open the store for a caller that already holds the store lock: the
/// locked_mutate publication seam. Skips the creation lock, because a
/// second flock on a fresh fd blocks behind the caller's own lock and
/// burns the full timeout on every write to a fresh graph.
pub(crate) fn open_holding_lock(graph: &Path) -> Result<Connection, String> {
    let path = database_path(graph);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    open_connection(graph)
}

fn open_connection(graph: &Path) -> Result<Connection, String> {
    let path = database_path(graph);
    let size = match path.metadata() {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => return Err(error.to_string()),
    };
    if size > 0 {
        let mut header = [0; 16];
        let mut file = std::fs::File::open(&path).map_err(|error| error.to_string())?;
        use std::io::Read;
        file.read_exact(&mut header)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if &header != b"SQLite format 3\0" {
            return Err(format!("{}: file is not a database", path.display()));
        }
    }
    let mut connection = Connection::open(&path).map_err(|error| error.to_string())?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(|error| error.to_string())?;
    // First opens of a new file race to switch it to WAL. Each upgrades a
    // read lock, and SQLite answers the loser busy at once, with no busy
    // handler, since waiting could deadlock. The loser goes on without
    // waiting: another open switches the file, and a connection that meets
    // a WAL file reads its header and uses WAL. A retry here waited out a
    // reader in this same process that could not finish until the open did.
    match connection.execute_batch("PRAGMA journal_mode=WAL;") {
        Err(rusqlite::Error::SqliteFailure(error, _))
            if error.code == rusqlite::ErrorCode::DatabaseBusy => {}
        outcome => outcome.map_err(|error| error.to_string())?,
    }
    connection
        .execute_batch("PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;")
        .map_err(|error| error.to_string())?;
    connection
        .execute_batch(&graph_meta_ddl())
        .map_err(|error| error.to_string())?;
    schema_v4::migrate_if_needed(&mut connection, graph)?;
    entities::ensure_table(&connection)?;
    nodes::ensure_table(&connection)?;
    sessions::ensure_table(&connection)?;
    comments::ensure_table(&connection)?;
    encounters::ensure_table(&connection)?;
    findings::ensure_table(&connection)?;
    pull_requests::ensure_table(&connection)?;
    relations::ensure_table(&connection)?;
    costs::ensure_table(&connection)?;
    decisions::ensure_table(&connection)?;
    ensure_triggers(&connection)?;
    search::ensure_table(&connection)?;
    import_if_needed(&mut connection)?;
    retire_graph_json(&connection, graph)?;
    decisions::import_if_needed(&mut connection, graph)?;
    archive_import_if_needed(&mut connection, graph)?;
    Ok(connection)
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
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
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
        // BOTH tables count as live: the working import parks an
        // unrepresentable seed row in nodes_raw, and a check against `nodes`
        // alone would fold the archived copy over it - the live row lost and
        // archived residency stamped onto an id that was still working.
        let exists: i64 = transaction
            .query_row(
                "SELECT (SELECT COUNT(*) FROM nodes WHERE id = ?1)
                      + (SELECT COUNT(*) FROM nodes_raw WHERE id = ?1)",
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
            let Some(id) = row.get("id").and_then(Value::as_str) else {
                continue;
            };
            // An unrepresentable row still takes ARCHIVE RESIDENCY: without
            // the stamp it answers default reads as a live node, exactly the
            // leak the archived_at stamp exists to prevent.
            let mut raw = row.clone();
            if raw.get("archived_at").is_none() {
                if let Some(obj) = raw.as_object_mut() {
                    obj.insert(
                        "archived_at".to_string(),
                        Value::String(crate::graph_store::now_isoformat()),
                    );
                }
            }
            nodes::save_raw(&transaction, id, next_ordinal, &raw)
                .map_err(|error| format!("archive import: {error}"))?;
            next_ordinal += 1;
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

/// The one-shot import of a legacy SQLite blob table. Schema upgrades are
/// handled by the owned migrations.
fn materialized_rows(connection: &Connection) -> Result<i64, String> {
    // Raw-carried rows count as materialized: a store holding only them is
    // NOT fresh, or every open would re-fold the seed over the carry.
    connection
        .query_row(
            "SELECT (SELECT COUNT(*) FROM nodes) + (SELECT COUNT(*) FROM nodes_raw)",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())
}

fn import_if_needed(connection: &mut Connection) -> Result<(), String> {
    if materialized_rows(connection)? > 0 {
        return Ok(());
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
    }
    if rows.is_empty() && !has_entries {
        // An empty-or-absent graph with no blob: the store is live from
        // birth, so the version stamp lands NOW. The old contract deferred
        // it to the first shadow write, which read as "the store has no
        // version" to every reader once reads answered sqlite only.
        //
        // The count is taken again under the write lock. A first write can
        // land between the unlocked count and this stamp, and stamping the
        // empty version over it lets the next writer's fence pass and
        // delete that write.
        let transaction = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        if materialized_rows(&transaction)? > 0 {
            return Ok(());
        }
        stamp_version_fields(&transaction, &content_version(&[]))?;
        stamp_meta(&transaction, "schema_version", SCHEMA_VERSION)?;
        return transaction.commit().map_err(|error| error.to_string());
    }
    // Dedup duplicate seed slugs before the saves: two rows carrying the
    // same slug collapse on the unique index, and INSERT OR REPLACE answers
    // that by silently deleting the earlier row - row loss on a first
    // import, not a quirk. The second occurrence takes -2, -3, ...
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
    // A legacy lock timestamp must land as its modeled stamp (locked_at)
    // before the saves: the columnar null would answer every read and the
    // derivation never fires again. ONLY this derivation runs pre-save --
    // the full defaults pass is a read-time overlay, and storing its
    // derived statuses (a blocker-held row reads blocked) reaches writes
    // and invents reclaim drift.
    for row in rows.iter_mut() {
        let Some(obj) = row.as_object_mut() else {
            continue;
        };
        if obj.contains_key("locked_at") {
            continue;
        }
        let Some(s) = obj.get("claimed_at").and_then(Value::as_str) else {
            continue;
        };
        if !s.trim().is_empty()
            && chrono::DateTime::parse_from_rfc3339(&s.replace('Z', "+00:00")).is_ok()
        {
            obj.insert("locked_at".to_string(), Value::String(s.to_string()));
        }
    }
    crate::graph_store::normalize_legacy_deferred(&mut rows);
    // IMMEDIATE, not deferred: a writer committing between this fold's first
    // read and its write trips SQLITE_BUSY_SNAPSHOT, which busy_timeout never
    // retries - the fold dies as "import: database is locked" under exactly
    // the contention its own writers create. Taking the write lock up front
    // makes the 5s busy window apply.
    let transaction = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    // Another opener may have folded the seed, and a writer published over
    // it, since the unlocked count. A second fold would revert that write.
    if materialized_rows(&transaction)? > 0 {
        return Ok(());
    }
    for (ordinal, row) in rows.iter().enumerate() {
        // A row the model cannot represent (a minimal legacy fixture row with
        // no slug/status) rides the raw carry verbatim: SQLite is the only
        // store, so a skip would be a silent loss.
        let Ok(mut node) = Node::from_json(row) else {
            let Some(id) = row.get("id").and_then(Value::as_str) else {
                continue;
            };
            nodes::save_raw(&transaction, id, ordinal as i64, row)
                .map_err(|error| format!("import: {error}"))?;
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

fn retire_graph_json(connection: &Connection, graph: &Path) -> Result<(), String> {
    if !graph.exists() {
        return Ok(());
    }
    let stored_rows: i64 = connection
        .query_row(
            "SELECT (SELECT COUNT(*) FROM nodes) + (SELECT COUNT(*) FROM nodes_raw)",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if stored_rows == 0 {
        let rows =
            match crate::graph_store::read_archive_raw(graph).map_err(|error| error.to_string())? {
                crate::graph_store::RawRead::Empty => Vec::new(),
                crate::graph_store::RawRead::Entries(rows) => rows,
                crate::graph_store::RawRead::MalformedRoot => {
                    return Err(format!(
                        "{} has no entries array and was never imported; refusing to retire it",
                        graph.display()
                    ));
                }
                crate::graph_store::RawRead::Corrupt(reason) => {
                    return Err(format!(
                    "{} was never imported and cannot be read ({reason}); refusing to retire it",
                    graph.display()
                ));
                }
            };
        if !rows.is_empty() {
            return Err(format!(
                "{} contains rows that were never imported; refusing to retire it",
                graph.display()
            ));
        }
    }
    let backup_dir = graph
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", graph.display()))?
        .join("backups");
    std::fs::create_dir_all(&backup_dir).map_err(|error| error.to_string())?;
    let retired = backup_dir.join(format!(
        "graph.json.retired.{}",
        crate::graph_store::backup_stamp()
    ));
    match std::fs::rename(graph, retired) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("cannot retire {}: {error}", graph.display())),
    }
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

/// The counter `backlog::api::version` serves. Read-only probe: an absent db
/// or key reads 0, never creates.
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

/// The last store version the canonical view pass rendered. `None` = nothing rendered
/// since the counter was born, so the next settled trigger owes a render.
pub fn rendered_version(graph: &Path) -> Result<Option<String>, String> {
    if !database_path(graph).exists() {
        return Ok(None);
    }
    let connection = open(graph)?;
    meta(&connection, "rendered_version")
}

/// Stamp the rendered marker. The render trigger runs outside any mutation,
/// so this writes graph metadata directly.
pub fn set_rendered_version(graph: &Path, value: &str) -> Result<(), String> {
    let connection = open(graph)?;
    stamp_meta(&connection, "rendered_version", value)
}

/// Publish rows to the sole graph store and return its content digest.
pub fn authoritative_sync(
    graph: &Path,
    before: &[Value],
    after: &[Value],
) -> Result<String, String> {
    // Same seam contract as authoritative_sync: the caller holds the store lock.
    let mut connection = open_holding_lock(graph)?;
    let transaction = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
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
        .query_row(
            "SELECT (SELECT COUNT(*) FROM nodes) + (SELECT COUNT(*) FROM nodes_raw)",
            [],
            |row| row.get(0),
        )
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
    // Raw-carried rows landed too: they answer reads verbatim.
    for batch in ids.chunks(SQLITE_BIND_BATCH) {
        let placeholders = std::iter::repeat_n("?", batch.len())
            .collect::<Vec<_>>()
            .join(",");
        let query = format!("SELECT id FROM nodes_raw WHERE id IN ({placeholders})");
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

/// The single-row mutation path: one `BEGIN IMMEDIATE` transaction reads
/// the CURRENT authoritative rows,
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
    let started = std::time::Instant::now();
    let (outcome, retries) = retry_on_busy(|| mutate_single_row_once(graph, mutation, &mut apply))?;
    if outcome {
        emit_gate_event(mutation, started.elapsed().as_millis(), retries);
    }
    Ok(outcome)
}

/// The one busy retry behind both graph write paths: the typed single-row
/// seam and the whole-graph publish. A busy or locked store sleeps with
/// linear backoff and retries; anything else surfaces immediately. A busy
/// error that survives the budget comes back as the honest refusal (AC5):
/// condition, attempts, elapsed, and the read-back command, still carrying
/// the `locked` substring every downstream busy-detect matches on. The
/// second tuple element is the retry count the gate event records.
pub(crate) fn retry_on_busy<T>(
    mut op: impl FnMut() -> Result<T, String>,
) -> Result<(T, u32), String> {
    const ATTEMPTS: usize = 3;
    let started = std::time::Instant::now();
    let mut retries = 0u32;
    for attempt in 0..ATTEMPTS {
        match op() {
            Ok(value) => return Ok((value, retries)),
            Err(error) => {
                let busy = error.contains("locked") || error.contains("busy");
                if busy && attempt + 1 < ATTEMPTS {
                    retries += 1;
                    std::thread::sleep(Duration::from_millis(100 * (attempt as u64 + 1)));
                    continue;
                }
                if busy {
                    return Err(format!(
                        "graph write refused: the store was busy after {ATTEMPTS} attempts over \
                         {:.1}s (sqlite: {error}). Nothing was written; no node was created. \
                         Run `fno backlog get <id>` to confirm, then retry.",
                        started.elapsed().as_secs_f32()
                    ));
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
    // The close-evidence rule at the single-row seam: judged over the
    // transaction's pre rows and the mutation's output, before anything is
    // written, so a refusal rolls back with the dropped transaction.
    crate::backlog::done_evidence::enforce(&rows, &working)?;
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
    findings::save(
        connection,
        &node.id,
        node.findings.as_deref().unwrap_or(&[]),
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
    costs::save(connection, &node.id, node.costs.as_deref().unwrap_or(&[]))?;
    Ok(())
}

/// Delete one node's whole row set through the owning modules.
fn delete_aggregate(connection: &Connection, id: &str) -> Result<(), String> {
    nodes::delete(connection, id)?;
    sessions::delete(connection, id)?;
    comments::delete(connection, id)?;
    encounters::delete(connection, id)?;
    findings::delete(connection, id)?;
    pull_requests::delete(connection, id)?;
    relations::delete(connection, id)?;
    costs::delete(connection, id)?;
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
            // A row moving between the carry and the typed tables is written
            // before its old copy is deleted: the insert reads the old
            // version, so the move never restarts it at 0.
            Some(body) => {
                let mut node = match Node::from_json(body) {
                    Ok(node) => node,
                    Err(_error) if strict => {
                        // SQLite is the only store: an unrepresentable row
                        // is CARRIED verbatim, never refused (the caller's
                        // write would lose data) and never dropped. The
                        // typed copy dies with the carry: one row per id.
                        crate::backlog::nodes::save_raw(
                            connection,
                            &id,
                            ordinals.get(id.as_str()).copied().unwrap_or(0),
                            body,
                        )?;
                        delete_aggregate(connection, &id)?;
                        report.present_ids.push(id);
                        continue;
                    }
                    Err(_) => continue,
                };
                node.ordinal = ordinals.get(id.as_str()).copied().unwrap_or(0);
                save_aggregate(connection, &node)?;
                // A row that leaves the raw carry (the model now represents
                // it) stops riding verbatim.
                crate::backlog::nodes::delete_raw(connection, &id)?;
                report.present_ids.push(id);
            }
            None => {
                delete_aggregate(connection, &id)?;
                crate::backlog::nodes::delete_raw(connection, &id)?;
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

/// The nodes a merge-grant op can use, in ordinal order, built through the
/// same single-node loader as the full export. With `pr`, every carrier of
/// that number; without, the queue's grant candidates' seq-0 numbers and
/// then every carrier of each. The queue filter stays the authority: the
/// SQL only has to return a superset of what it keeps, so a raw-status
/// filter here is safe (STATUS_MIGRATION never produces `done` or
/// `superseded`, and the readiness overlay passes both through).
pub fn read_pr_entries(graph: &Path, pr: Option<i64>) -> Result<Vec<Value>, String> {
    let connection = open(graph)?;
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    if meta(&transaction, "version")?.is_none() {
        return Err("SQLite graph has no version".into());
    }
    let numbers: Vec<i64> = match pr {
        Some(number) => vec![number],
        None => {
            let mut statement = transaction
                .prepare(
                    "SELECT DISTINCT p.number FROM pull_requests p
                     JOIN nodes n ON n.id = p.node_id
                     WHERE p.seq = 0 AND p.number IS NOT NULL
                       AND n.status NOT IN ('done', 'superseded')
                       AND (p.merge_status IS NULL
                            OR (p.merge_status <> 'merged' AND p.merge_status <> 'closed'))
                       AND EXISTS (SELECT 1 FROM sessions s WHERE s.node_id = n.id
                                   AND s.phase = 'do'
                                   AND s.merge_grant IS NOT NULL
                                   AND s.merge_grant <> 'null')",
                )
                .map_err(|error| error.to_string())?;
            let rows = statement
                .query_map([], |row| row.get(0))
                .map_err(|error| error.to_string())?
                .collect::<Result<Vec<i64>, _>>()
                .map_err(|error| error.to_string())?;
            rows
        }
    };
    let mut entries = Vec::new();
    if numbers.is_empty() {
        return Ok(entries);
    }
    let placeholders = numbers
        .iter()
        .enumerate()
        .map(|(index, _)| format!("?{}", index + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let like_base = numbers.len();
    let url_likes = numbers
        .iter()
        .enumerate()
        .map(|(index, _)| format!("p.url LIKE ?{}", like_base + index + 1))
        .collect::<Vec<_>>()
        .join(" OR ");
    // The url arm keeps a PR row that carries the number only in its url
    // (number NULL), which node_carries_pr would still match. The LIKE may
    // over-match (/pull/11 also names /pull/110); a superset is the
    // invariant - the caller's filter re-checks each row precisely.
    let sql = format!(
        "SELECT DISTINCT n.id, n.ordinal FROM nodes n
         JOIN pull_requests p ON p.node_id = n.id
         WHERE p.number IN ({placeholders})
            OR (p.number IS NULL AND p.url IS NOT NULL AND ({url_likes}))
         ORDER BY n.ordinal, n.id"
    );
    let mut params: Vec<rusqlite::types::Value> = numbers
        .iter()
        .map(|n| rusqlite::types::Value::from(*n))
        .collect();
    params.extend(
        numbers
            .iter()
            .map(|n| rusqlite::types::Value::from(format!("%/pull/{n}%"))),
    );
    let mut statement = transaction
        .prepare(&sql)
        .map_err(|error| error.to_string())?;
    let ids = statement
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            row.get::<_, String>(0)
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    for id in ids {
        if let Some(node) = nodes::load(&transaction, &id)? {
            entries.push(node.to_json());
        }
    }
    Ok(entries)
}

/// The rows behind an open connection, in ordinal order. One scan per
/// table; the batched assembler shares every row mapper with the
/// single-node load.
pub fn export_rows(connection: &Connection) -> Result<Vec<Value>, String> {
    if meta(connection, "version")?.is_none() {
        return Err("SQLite graph has no version".into());
    }
    // Project the external claim store once for the whole export: the claim
    // is the holder of record, and the stored lock fields are the retired
    // mirror. Loading each node through `nodes::load` would rescan every
    // lockfile for every row.
    let node_claims = nodes::node_claims_by_id()?;
    let mut statement = connection
        .prepare("SELECT id, ordinal FROM nodes ORDER BY ordinal, id")
        .map_err(|error| error.to_string())?;
    let ids = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(|error| error.to_string())?;
    let mut typed: Vec<(i64, String, Value)> = Vec::new();
    for id in ids {
        let (id, ordinal) = id.map_err(|error| error.to_string())?;
        let claim = node_claims.get(&id).cloned().unwrap_or_default();
        let Some(node) = nodes::load_with_claim(&connection, &id, Some(claim))? else {
            return Err(format!("node {id} vanished mid-export"));
        };
        typed.push((ordinal, id, node.to_json()));
    }
    // Raw-carried rows round-trip verbatim, merged into ordinal order.
    let mut merged: Vec<(i64, String, Value)> = nodes::raw_rows(connection)?
        .into_iter()
        .map(|(id, ordinal, body)| (ordinal, id, body))
        .collect();
    for (_, id, body) in &mut merged {
        let claim = node_claims.get(id).cloned().unwrap_or_default();
        nodes::project_claim_value(body, claim);
    }
    merged.append(&mut typed);
    merged.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    Ok(merged.into_iter().map(|(_, _, row)| row).collect())
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

/// Each row's stored write count, restricted to `ids` when given: the map
/// the keeper's begin hands out and commit_rows compares (see
/// [`nodes::versions`]).
pub fn row_versions(
    graph: &Path,
    ids: Option<&[&str]>,
) -> Result<std::collections::BTreeMap<String, i64>, String> {
    let connection = open(graph)?;
    nodes::versions(&connection, ids)
}

pub fn version(graph: &Path) -> Result<String, String> {
    let connection = open(graph)?;
    meta(&connection, "version")?.ok_or_else(|| "SQLite graph has no version".into())
}

pub fn export_status(graph: &Path) -> Result<String, String> {
    version(graph)
}

/// At most one `graph.db.<stamp>` snapshot per hour in `backups/`, keeping
/// the newest [`crate::graph_store::GRAPH_BACKUP_KEEP`]. VACUUM INTO also
/// compacts; its target must not
/// exist, so the microsecond stamp names it. The hour gate lives in
/// `graph_meta.last_snapshot_ms`, not file mtimes, so a moved or inspected
/// snapshot cannot skew the clock.
pub(crate) fn snapshot_db(graph: &Path, now: u128) -> Result<(), String> {
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

fn sorted_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: std::collections::BTreeMap<String, Value> = map
                .iter()
                .map(|(key, value)| (key.clone(), sorted_value(value)))
                .collect();
            serde_json::to_value(sorted).unwrap_or(Value::Null)
        }
        Value::Array(rows) => Value::Array(rows.iter().map(sorted_value).collect()),
        _ => value.clone(),
    }
}

fn canonical_row(row: &Value) -> String {
    crate::graph_store::to_python_json(&sorted_value(&model::strip_nulls_value(row)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fixture(name: &str) -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let graph = dir.path().join(name);
        (dir, graph)
    }

    /// A first write that lands between an opener's unlocked row count and
    /// its empty-store stamp keeps its version. The stamp used to reset it
    /// to the empty hash, so the next writer's fence passed and its publish
    /// deleted the write.
    #[test]
    fn an_opener_never_restamps_the_empty_version_over_a_first_write() {
        let (_dir, graph) = fixture("graph.json");
        drop(open(&graph).unwrap());
        let mut writer = open(&graph).unwrap();
        let transaction = writer
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        nodes::save_raw(&transaction, "x-a", 0, &serde_json::json!({"id": "x-a"})).unwrap();
        stamp_version(&transaction, "sqlite:first-write").unwrap();
        let opener = {
            let graph = graph.clone();
            std::thread::spawn(move || open(&graph).map(drop))
        };
        std::thread::sleep(Duration::from_millis(500));
        transaction.commit().unwrap();
        opener.join().unwrap().unwrap();
        assert_eq!(version(&graph).unwrap(), "sqlite:first-write");
    }

    /// First opens of a brand-new store, all at once, all succeed.
    #[test]
    fn concurrent_first_opens_of_a_new_store_all_succeed() {
        let mut failures = Vec::new();
        for _ in 0..50 {
            let (_dir, graph) = fixture("graph.json");
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(6));
            let openers: Vec<_> = (0..6)
                .map(|_| {
                    let (graph, barrier) = (graph.clone(), barrier.clone());
                    std::thread::spawn(move || {
                        barrier.wait();
                        open(&graph).map(drop)
                    })
                })
                .collect();
            for opener in openers {
                if let Err(error) = opener.join().unwrap() {
                    failures.push(error);
                }
            }
        }
        assert!(failures.is_empty(), "{failures:?}");
    }

    fn two_node_graph(dir: &TempDir) -> PathBuf {
        let graph = dir.path().join("graph.json");
        let rows: Value = serde_json::from_str(
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
        crate::graph_store::seed_rows(&graph, rows["entries"].as_array().unwrap()).unwrap();
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
    fn backlog_schema_write_touches_only_changed_nodes() {
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
        // would strand these as orphans. node_claims is absent by design:
        // the claim mirror retires, and ensure_table drops the table on
        // every open, so there is nothing left to strand.
        for table in [
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
        read_entries(graph).unwrap()
    }

    #[test]
    fn flipgate_store_normalization_only_change_reaches_the_store() {
        // AC1-HP: the seeded row lacks the default lists; a mutation on a
        // DIFFERENT node publishes the defaulted form. The diff must see
        // that change against the raw baseline and save the row.
        let dir = TempDir::new().unwrap();
        let graph = two_node_graph(&dir);
        let mut raw = raw_rows(&graph);
        let one = raw[0].as_object_mut().unwrap();
        for key in [
            "tags",
            "locked_by",
            "locked_at",
            "dispatch_verb",
            "sessions",
        ] {
            one.remove(key);
        }
        // Keep the raw row un-defaulted and unlocked so owner normalization
        // cannot change its status while this test isolates missing tags.
        crate::graph_store::seed_rows(&graph, &raw).unwrap();
        let mut after = raw.clone();
        // The Python mutator sends defaulted rows: ab-one gains "tags": [].
        crate::graph_store::apply_defaults(&mut after, false);
        assert_eq!(after[0]["tags"], Value::Array(vec![]));
        // A change on the other node is what triggers the publish.
        after[1]
            .as_object_mut()
            .unwrap()
            .insert("title".to_string(), Value::String("Two changed".into()));
        let outcome = crate::graph_store::locked_mutate_with_hook(
            &graph,
            crate::graph_store::MutateInput {
                entries: after.clone(),
                canonical_path: None,
                base_version: crate::graph_store::base_version(&graph).unwrap(),
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
        // graph.db is the only store; graph.json is not written.
        let stored = read_entries(&graph).unwrap();
        let one = stored
            .iter()
            .find(|e| e.get("id") == Some(&Value::String("ab-one".into())))
            .unwrap();
        assert_eq!(
            one.get("tags"),
            Some(&Value::Array(vec![])),
            "defaulted row reached the store"
        );
        let two = stored
            .iter()
            .find(|e| e.get("id") == Some(&Value::String("ab-two".into())))
            .unwrap();
        assert_eq!(two.get("title"), Some(&Value::String("Two changed".into())));
    }

    #[test]
    fn flipgate_store_superseded_settle_reaches_the_store() {
        // AC2-HP: the raw row is blocked with superseded_by set; the
        // mutation pipeline settles it to superseded. The settle must reach
        // the store, not only the published rows.
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
        crate::graph_store::seed_rows(&graph, &raw).unwrap();
        // Mutate the OTHER node; the pipeline settles ab-two itself.
        let mut after = raw.clone();
        after[0]
            .as_object_mut()
            .unwrap()
            .insert("title".to_string(), Value::String("One changed".into()));
        let outcome = crate::graph_store::locked_mutate_with_hook(
            &graph,
            crate::graph_store::MutateInput {
                entries: after.clone(),
                canonical_path: None,
                base_version: crate::graph_store::base_version(&graph).unwrap(),
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
        // graph.db is the only store; read the settled row back from it.
        let stored = read_entries(&graph).unwrap();
        let two = stored
            .iter()
            .find(|e| e.get("id") == Some(&Value::String("ab-two".into())))
            .unwrap();
        assert_eq!(
            two.get("status"),
            Some(&Value::String("superseded".into())),
            "the settle reached the store"
        );
    }

    #[test]
    fn flipgate_store_write_changed_counts_only_canonical_changes() {
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
    fn an_unrepresentable_row_is_carried_verbatim_by_the_authoritative_publish() {
        // SQLite is the only store: a publish that cannot represent a row
        // still records it (raw carry) and stamps a version. There is no
        // json leg left to hold the row, so refusing would lose data.
        let dir = TempDir::new().unwrap();
        let graph = two_node_graph(&dir);
        let before = raw_rows(&graph);
        let mut after = before.clone();
        after[0]["status"] = Value::String("not-a-status".into());

        let version = authoritative_sync(&graph, &before, &after).unwrap();

        assert!(!version.is_empty(), "the publish stamped a version");
        let stored = read_entries(&graph).unwrap();
        let carried = stored
            .iter()
            .find(|e| e.get("id") == Some(&Value::String("ab-one".into())))
            .expect("the row survived the publish");
        assert_eq!(
            carried["status"], "not-a-status",
            "the raw row rides verbatim"
        );
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
    fn authoritative_publish_rejects_a_row_without_a_usable_id() {
        let dir = TempDir::new().unwrap();
        let graph = two_node_graph(&dir);
        let before = raw_rows(&graph);
        let mut after = before.clone();
        after.push(serde_json::json!({"title": "missing id"}));

        let error = authoritative_sync(&graph, &before, &after).unwrap_err();

        assert!(error.contains("ordinal 2"));
        assert!(error.contains("missing string id"));
    }

    #[test]
    fn authoritative_publish_rejects_duplicate_ids() {
        let dir = TempDir::new().unwrap();
        let graph = two_node_graph(&dir);
        let before = raw_rows(&graph);
        let mut after = before.clone();
        after.push(after[0].clone());

        let error = authoritative_sync(&graph, &before, &after).unwrap_err();

        assert!(error.contains("duplicate id"));
        assert!(error.contains("ab-one"));
    }

    #[test]
    fn a_landed_publish_reads_every_id_back() {
        let dir = TempDir::new().unwrap();
        let graph = two_node_graph(&dir);
        let before = raw_rows(&graph);
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
    fn authoritative_publish_rejects_a_missing_unchanged_row() {
        let dir = TempDir::new().unwrap();
        let graph = two_node_graph(&dir);
        let before = raw_rows(&graph);
        let connection = open(&graph).unwrap();
        delete_aggregate(&connection, "ab-two").unwrap();
        drop(connection);
        let mut after = before.clone();
        after[0]["title"] = Value::String("One renamed".into());

        let error = authoritative_sync(&graph, &before, &after).unwrap_err();

        assert!(error.contains("ab-two"));
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
        let rows: Value = serde_json::from_str(
            r#"{"entries": [
                {"id": "ab-one", "slug": "ab-one", "title": "One", "type": "feature",
                 "status": "idea", "priority": "p2", "domain": "code"},
                {"id": "ab-two", "slug": "ab-two", "title": "Two", "type": "feature",
                 "status": "ready", "priority": "p2", "domain": "code"}
            ]}"#,
        )
        .unwrap();
        crate::graph_store::seed_rows(&graph, rows["entries"].as_array().unwrap()).unwrap();
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
        let text = crate::events::committed_journal_text(&journal);
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
    fn a_committer_inside_the_publish_window_does_not_refuse_the_publish() {
        // AC4-EDGE: another writer holds the write lock and commits while
        // the authoritative publish runs. On IMMEDIATE the publish waits on
        // the lock, reads AFTER the interleaved commit, and lands. On the
        // deferred shape this replaces, the publish's read pinned the
        // pre-commit snapshot and the upgrade refused the instant the lock
        // freed (the measured 0.0000s SQLITE_BUSY): the failure this test
        // exists to keep dead.
        let _env_lock = crate::claims::test_env_lock().lock().unwrap();
        let spaces = tempfile::TempDir::new().unwrap();
        declare_test_roots(spaces.path());
        let (_dir, graph) = seeded_sqlite_fixture();
        let before = export_rows(&open(&graph).unwrap()).unwrap();
        let mut after = before.clone();
        if let Some(row) = after.first_mut() {
            row["title"] = serde_json::json!("published-under-contention");
        }

        // The interleaving writer: takes the write lock, holds it past the
        // publish's begin, commits mid-publish.
        let graph_for_writer = graph.clone();
        let writer = std::thread::spawn(move || {
            let mut connection = open(&graph_for_writer).unwrap();
            let transaction = connection
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .unwrap();
            transaction
                .execute(
                    "INSERT INTO graph_meta(key, value) VALUES ('ac4_probe', 'held')",
                    [],
                )
                .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(300));
            drop(transaction);
        });
        // Let the writer take its lock before the publish starts.
        std::thread::sleep(std::time::Duration::from_millis(80));

        let version = authoritative_sync(&graph, &before, &after)
            .expect("the publish must wait out the interleaving writer and commit");
        writer.join().unwrap();
        assert!(!version.is_empty());
        let rows = export_rows(&open(&graph).unwrap()).unwrap();
        assert_eq!(
            rows.first()
                .and_then(|row| row.get("title"))
                .and_then(|t| t.as_str()),
            Some("published-under-contention")
        );
    }

    #[test]
    fn single_row_write_uses_current_store_state() {
        let _env_lock = crate::claims::test_env_lock().lock().unwrap();
        let spaces = tempfile::TempDir::new().unwrap();
        declare_test_roots(spaces.path());
        let (_dir, graph) = seeded_sqlite_fixture();
        // The single-row path sees and mutates a node already in the store.
        mutate_single_row(&graph, "node_create", |rows| {
            rows.push(serde_json::json!({
                "id": "ab-flip", "slug": "ab-flip", "title": "Flip", "type": "feature",
                "status": "idea", "priority": "p2", "domain": "code"
            }));
            Ok(true)
        })
        .unwrap();
        assert!(!graph.exists(), "writes do not create a graph.json mirror");
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
