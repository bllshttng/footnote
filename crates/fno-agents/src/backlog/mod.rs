//! The backlog store: graph.db lifecycle (schema, import and sync). Each aggregate's tables live in
//! its owning module and no other file writes them (ruling 4);
//! `TABLE_OWNERS` names the owners and the table_ownership test enforces
//! Every mutation writes only the changed nodes' rows in one transaction.

pub mod advance_fill;
pub mod api;
pub mod autolink;
pub(crate) mod binding;
pub mod birth;
pub mod cli;
pub(crate) mod closures;
pub mod collision;
pub mod commands;
pub mod comments;
pub mod costs;
pub mod create_cli;
pub mod decisions;
pub mod decisions_cli;
pub mod done_evidence;
pub(crate) mod drift_emit;
pub(crate) mod drift_scan;
pub mod edges;
pub mod encounters;
pub mod entities;
pub mod epic_cap;
pub mod fields;
pub mod find_cli;
pub mod findings;
pub mod get_cli;
pub mod idea_cap;
pub(crate) mod merge_evidence;
pub(crate) mod merge_state;
pub mod model;
pub mod next;
pub mod node_ref;
pub mod node_state;
pub mod nodes;
pub mod note_cli;
pub mod note_history;
pub mod note_migrate;
pub mod note_stale;
pub mod orphan_plans;
pub mod patch;
pub mod pr_link;
pub(crate) mod promise;
pub mod pull_requests;
pub mod rank_cli;
pub mod receipt;
pub(crate) mod reconcile_cli;
pub mod relatedness;
pub mod relations;
pub mod render;
pub mod schema_v4;
pub mod search;
pub mod session_cli;
pub mod sessions;
pub mod settings;
pub mod style_check;
pub(crate) mod supersession;
pub mod target_binding;
pub mod title_gate;
pub mod undispatched;
pub mod update_cli;
pub mod worked;
pub mod workflows;

use crate::backlog::model::Node;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Schema 4 (see schema_v4.rs) is the shape every table is born in.
pub const SCHEMA_VERSION: &str = "4";
const SCHEMA_VERSION_NUMBER: u32 = 4;
const OPEN_SETUP_VERSION: &str = "2";
const OPEN_SETUP_VERSION_NUMBER: u32 = 2;

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
    ("edges", "backlog/edges.rs"),
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

/// The store behind a graph anchor. The state-root spellings (the anchor at
/// the root, or its `db/` twin) resolve through the layout table, so the
/// legacy and new spellings reach one physical file; every other parent
/// (a space, a test fixture) keeps the sibling store.
pub fn database_path(graph: &Path) -> PathBuf {
    anchor_path(graph).with_extension("db")
}

/// The canonical anchor spelling behind any graph spelling: the same walk
/// `database_path` makes, without the db substitution. The journal anchors
/// here, so a writer holding the anchor and a reader holding the db twin
/// derive one journal file.
pub fn anchor_path(graph: &Path) -> PathBuf {
    let name = match graph.file_name().and_then(|n| n.to_str()) {
        Some(n) => n.to_string(),
        None => return graph.with_extension("json"),
    };
    // The db twins of the anchors are spellings of the same graph: a reader
    // holding graph.db must land where a writer holding graph.json wrote,
    // so they take the same walk instead of a naive sibling rename.
    let anchor = match name.as_str() {
        "graph.json" | "graph.db" => "graph.json",
        "graph-archive.json" | "graph-archive.db" => "graph-archive.json",
        _ => return graph.with_extension("json"),
    };
    let parent = match graph.parent() {
        Some(p) => p,
        None => return graph.with_extension("json"),
    };
    let root = if parent.file_name().is_some_and(|n| n == "db") {
        match parent.parent() {
            Some(r) => r,
            None => return graph.with_extension("json"),
        }
    } else {
        parent
    };
    crate::state_layout::place(root, anchor)
}

/// The state root an anchor belongs to: the same walk `database_path` does
/// behind its db/ parent check, so the migration fence resolves from the
/// same place the move wrote it.
fn state_root_of(graph: &Path) -> &Path {
    let parent = graph.parent().unwrap_or(graph);
    if parent.file_name().is_some_and(|n| n == "db") {
        parent.parent().unwrap_or(parent)
    } else {
        parent
    }
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
    open_connection(graph)
}

fn open_connection(graph: &Path) -> Result<Connection, String> {
    // A migration publishing under this root parks the legacy inode we
    // would otherwise open; the bounded fence wait orders us after it.
    crate::state_layout_sqlite::wait_for_fence(state_root_of(graph));
    let mut connection = crate::store_conn::open_write(&database_path(graph))?;
    connection
        .execute_batch("PRAGMA foreign_keys=ON;")
        .map_err(|error| error.to_string())?;
    if !schema_needs_ensure(&connection)? {
        // A stamped store can still park legacy blob rows (a seed landing
        // after the one-time setup): the fold self-gates on materialized
        // rows, so a healthy store pays one COUNT here and a parked store
        // folds. The DDL, migrations and one-shot imports below stay
        // setup-only.
        //
        // A store stamped before the identity columns landed carries no
        // stamp gap, so the ensure path below never runs for it and only
        // this migration moves it to the current shape; without it the first
        // reader of agent_sessions.fno_id dies on the missing column.
        entities::migrate_identity(&connection)?;
        import_if_needed(&mut connection)?;
        if archive_needs_import(&connection, graph)? {
            archive_import_if_needed(&mut connection, graph)?;
        }
        return Ok(connection);
    }
    connection
        .execute_batch(&graph_meta_ddl())
        .map_err(|error| error.to_string())?;
    schema_v4::migrate_if_needed(&mut connection, graph)?;
    entities::ensure_table(&connection)?;
    entities::migrate_identity(&connection)?;
    nodes::ensure_table(&connection)?;
    sessions::ensure_table(&connection)?;
    comments::ensure_table(&connection)?;
    encounters::ensure_table(&connection)?;
    findings::ensure_table(&connection)?;
    pull_requests::ensure_table(&connection)?;
    relations::ensure_table(&connection)?;
    costs::ensure_table(&connection)?;
    decisions::ensure_table(&connection)?;
    edges::ensure_table(&connection)?;
    ensure_triggers(&connection)?;
    search::ensure_table(&connection)?;
    import_if_needed(&mut connection)?;
    retire_graph_json(&connection, graph)?;
    decisions::import_if_needed(&mut connection, graph)?;
    archive_import_if_needed(&mut connection, graph)?;
    stamp_meta(&connection, "open_setup_version", OPEN_SETUP_VERSION)?;
    Ok(connection)
}

fn schema_needs_ensure(connection: &Connection) -> Result<bool, String> {
    let versions = connection.query_row(
        "SELECT
             (SELECT value FROM graph_meta WHERE key = 'schema_version'),
             (SELECT value FROM graph_meta WHERE key = 'open_setup_version')",
        [],
        |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
            ))
        },
    );
    let (schema, setup) = match versions {
        Ok(versions) => versions,
        Err(error) if error.to_string().contains("no such table") => return Ok(true),
        Err(error) => return Err(error.to_string()),
    };
    Ok(version_is_below(schema, SCHEMA_VERSION_NUMBER)
        || version_is_below(setup, OPEN_SETUP_VERSION_NUMBER))
}

fn version_is_below(version: Option<String>, expected: u32) -> bool {
    version
        .and_then(|value| value.parse::<u32>().ok())
        .map_or(true, |version| version < expected)
}

/// True when a stamped store still parks legacy blob rows: the one-time
/// setup completed before the rows landed, so only a fold shows them. A
/// folded store has no `entries` table, and a store born stamped has none
/// either, so every healthy live store answers false. Read-only: a read
/// connection runs this to decide whether it must fall back to the write
/// open, and pays three cheap queries when it does not.
fn store_owes_a_fold(connection: &Connection) -> Result<bool, String> {
    let has_entries: bool = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'entries'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|count| count > 0)
        .map_err(|error| error.to_string())?;
    if !has_entries {
        return Ok(false);
    }
    let parked: i64 = connection
        .query_row("SELECT COUNT(*) FROM entries", [], |row| row.get(0))
        .map_err(|error| error.to_string())?;
    if parked == 0 {
        return Ok(false);
    }
    Ok(materialized_rows(connection)? == 0)
}

fn read_connection(graph: &Path) -> Result<Connection, String> {
    crate::state_layout_sqlite::wait_for_fence(state_root_of(graph));
    let path = database_path(graph);
    if path.exists() {
        let connection = crate::store_conn::open_read(&path)?;
        if !schema_needs_ensure(&connection)?
            && !store_owes_a_fold(&connection)?
            && !archive_needs_import(&connection, graph)?
        {
            return Ok(connection);
        }
        drop(connection);
    }
    drop(open(graph)?);
    crate::store_conn::open_read(&path)
}

fn archive_needs_import(connection: &Connection, graph: &Path) -> Result<bool, String> {
    Ok(graph.with_file_name("graph-archive.json").exists()
        && meta(connection, "archive_imported_v2")?.is_none())
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
    // An anchor that names the database itself (`--graph .../graph.db`) maps
    // to itself, so renaming it would retire the live store.
    if !graph.exists() || database_path(graph) == graph {
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
    let connection = crate::store_conn::open_read(&database_path(graph))?;
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
    let connection = read_connection(graph)?;
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
    // hold here too: an empty presence field only when the write introduces
    // it, a slug on every row, and a touched_at stamp when a row's curation
    // fields moved.
    crate::graph_store::refuse_new_empty_presence(&rows, &working)?;
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
    let connection = read_connection(graph)?;
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
    let connection = read_connection(graph)?;
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
                                   AND s.phase = 'execute'
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

/// The whole canonical export in store order, including raw-carried and archived rows.
pub fn export_rows(connection: &Connection) -> Result<Vec<Value>, String> {
    query_rows(connection, &RowQuery::default(), None)
}

/// Narrow selection and projection. Whole-read defaults retain archived rows;
/// readiness support stays internal when requested through `with_blockers`.
#[derive(Clone, Debug)]
pub struct RowQuery {
    pub filter: api::NodeFilter,
    pub fields: Option<Vec<String>>,
    pub include_archived: bool,
    pub with_blockers: bool,
}

impl Default for RowQuery {
    fn default() -> Self {
        Self {
            filter: Default::default(),
            fields: None,
            include_archived: true,
            with_blockers: false,
        }
    }
}

/// Query rows after defaults, exact filtering, and projection. The empty query
/// preserves the raw export; hidden blocker and child support never escapes.
pub fn read_entries_where(graph: &Path, query: &RowQuery) -> Result<Vec<Value>, String> {
    query_rows(&read_connection(graph)?, query, None)
}

pub(crate) fn read_entries_where_defaulted(
    graph: &Path,
    query: &RowQuery,
    keep_malformed: bool,
) -> Result<Vec<Value>, String> {
    query_rows(&read_connection(graph)?, query, Some(keep_malformed))
}

/// The old whole-export enumeration, without materializing any node bodies.
pub fn row_ordinals(graph: &Path) -> Result<std::collections::HashMap<String, i64>, String> {
    let connection = read_connection(graph)?;
    let mut statement = connection
        .prepare(
            "SELECT id FROM (SELECT id, ordinal FROM nodes UNION ALL
         SELECT id, ordinal FROM nodes_raw WHERE json_type(body) = 'object') ORDER BY ordinal, id",
        )
        .map_err(|error| error.to_string())?;
    let ids = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?;
    ids.enumerate()
        .map(|(ordinal, id)| id.map(|id| (id, ordinal as i64)))
        .collect::<Result<_, _>>()
        .map_err(|error| error.to_string())
}

fn query_rows(
    connection: &Connection,
    query: &RowQuery,
    keep_malformed: Option<bool>,
) -> Result<Vec<Value>, String> {
    let transaction = if connection.is_autocommit() {
        Some(
            connection
                .unchecked_transaction()
                .map_err(|error| error.to_string())?,
        )
    } else {
        None
    };
    let connection = transaction
        .as_ref()
        .map(|transaction| &**transaction)
        .unwrap_or(connection);
    if meta(connection, "version")?.is_none() {
        return Err("SQLite graph has no version".into());
    }
    let filter = &query.filter;
    let mut predicates = Vec::new();
    let mut params: Vec<rusqlite::types::Value> = Vec::new();
    let mut in_values = |column: &str, values: &[String]| {
        let slots = values
            .iter()
            .map(|value| {
                params.push(value.clone().into());
                format!("?{}", params.len())
            })
            .collect::<Vec<_>>()
            .join(",");
        format!("{column} IN ({slots})")
    };
    if let Some(ids) = &filter.id_in {
        predicates.push(format!(
            "({} OR {})",
            in_values("id COLLATE NOCASE", ids),
            in_values("slug COLLATE NOCASE", ids)
        ));
    }
    if let Some(statuses) = &filter.status_in {
        let mut statuses: Vec<String> = statuses
            .iter()
            .map(|status| {
                if status == "claimed" {
                    "in_progress".to_owned()
                } else {
                    status.clone()
                }
            })
            .collect();
        if statuses.iter().any(|status| status == "in_progress") {
            statuses.extend(["claimed", "idea", "ready"].map(str::to_owned));
        }
        let expression = in_values("status", &statuses);
        predicates.push(if statuses.iter().any(|status| status == "blocked") {
            format!("({expression} OR status NOT IN ('done','deferred','superseded','in_review'))")
        } else {
            expression
        });
    }
    for (column, value) in [("project", &filter.project), ("parent_id", &filter.parent)] {
        if let Some(value) = value {
            params.push(value.clone().into());
            predicates.push(format!("{column} = ?{}", params.len()));
        }
    }
    if filter.state_type.as_deref() == Some("open") {
        predicates.push("status NOT IN ('done','deferred','superseded')".into());
    }
    if !query.include_archived {
        predicates.push("archived_at IS NULL".into());
    }
    let where_sql = if predicates.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", predicates.join(" AND "))
    };
    let node_claims = nodes::node_claims_by_id()?;
    let mut fields = query.fields.clone();
    if let Some(fields) = &mut fields {
        if filter.session_id.is_some() {
            fields.push("sessions".into());
        }
        if query.with_blockers {
            fields.extend(
                ["blocked_by", "superseded_by", "supersession", "deferred_at"].map(str::to_owned),
            );
        }
    }
    let load = |id: &str, fields: Option<&[String]>| -> Result<Option<Value>, String> {
        let claim = node_claims.get(id).cloned().unwrap_or_default();
        let mut row = match nodes::load_with_claim(connection, id, Some(claim.clone()), fields)? {
            Some(node) => Some(node.to_json()),
            None => nodes::raw_rows_where(connection, Some(&[id.to_owned()]))?
                .into_iter()
                .find(|(raw_id, _, _)| raw_id == id)
                .map(|(_, _, row)| row),
        };
        if let Some(row) = &mut row {
            nodes::project_claim_value(row, claim);
        }
        Ok(row)
    };
    let mut statement = connection
        .prepare(&format!(
            "SELECT id, ordinal FROM nodes{where_sql} ORDER BY ordinal, id"
        ))
        .map_err(|error| error.to_string())?;
    let ids = statement
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    let mut merged = Vec::new();
    for (id, ordinal) in ids {
        let body = load(&id, fields.as_deref())?
            .ok_or_else(|| format!("node {id} vanished mid-export"))?;
        merged.push((ordinal, id, body));
    }
    if filter.only_ids() {
        for (id, ordinal, mut body) in nodes::raw_rows_where(connection, filter.id_in.as_deref())? {
            if !query.include_archived
                && body
                    .get("archived_at")
                    .is_some_and(|value| !value.is_null())
            {
                continue;
            }
            nodes::project_claim_value(
                &mut body,
                node_claims.get(&id).cloned().unwrap_or_default(),
            );
            merged.push((ordinal, id, body));
        }
    }
    merged.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let raw_export = keep_malformed.is_none()
        && filter.is_empty()
        && query.include_archived
        && query.fields.is_none()
        && !query.with_blockers;
    if raw_export {
        return Ok(merged.into_iter().map(|(_, _, row)| row).collect());
    }
    let mut rows: Vec<Value> = merged.into_iter().map(|(_, _, row)| row).collect();
    let asked_count = rows.len();
    let support_fields: Vec<String> = crate::graph_store::CHILD_SUMMARY_FIELDS
        .iter()
        .chain(
            [
                "parent",
                "completed_at",
                "deferred_at",
                "blocked_by",
                "superseded_by",
                "locked_by",
                "locked_by_harness",
                "locked_by_harness_session",
                "locked_at",
                "session_id",
            ]
            .iter(),
        )
        .map(|field| (*field).into())
        .collect();
    let mut loaded: std::collections::HashSet<String> = rows
        .iter()
        .filter_map(|row| row.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    // Child summaries need the omitted direct children, even on a filtered full read.
    if (!filter.is_empty() || !query.include_archived)
        && query
            .fields
            .as_ref()
            .is_none_or(|fields| fields.iter().any(|field| field == "children"))
    {
        let parents: Vec<String> = loaded.iter().cloned().collect();
        for (id, _, mut body) in nodes::raw_children(connection, &parents)? {
            if loaded.insert(id.clone()) {
                nodes::project_claim_value(
                    &mut body,
                    node_claims.get(&id).cloned().unwrap_or_default(),
                );
                rows.push(body);
            }
        }
        for parent in parents {
            let mut statement = connection
                .prepare_cached("SELECT id FROM nodes WHERE parent_id = ?1 ORDER BY ordinal, id")
                .map_err(|error| error.to_string())?;
            let ids = statement
                .query_map(params![parent], |row| row.get::<_, String>(0))
                .map_err(|error| error.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?;
            for id in ids {
                if loaded.insert(id.clone()) {
                    if let Some(row) = load(&id, Some(&support_fields))? {
                        rows.push(row);
                    }
                }
            }
        }
    }
    if query.with_blockers && !(filter.is_empty() && query.include_archived) {
        let blockers: Vec<String> = rows
            .iter()
            .flat_map(|row| {
                row.get("blocked_by")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
            })
            .filter_map(|id| id.as_str().map(str::to_owned))
            .collect();
        for blocker in blockers {
            let mut current = Some(blocker);
            let mut seen = std::collections::HashSet::new();
            for _ in 0..=crate::graph_store::MAX_CHAIN_HOPS {
                let Some(id) = current.take() else {
                    break;
                };
                if !seen.insert(id.clone()) {
                    break;
                }
                if loaded.insert(id.clone()) {
                    if let Some(row) = load(&id, Some(&support_fields))? {
                        rows.push(row);
                    }
                }
                current = rows
                    .iter()
                    .find(|row| row.get("id").and_then(Value::as_str) == Some(&id))
                    .and_then(|row| row.get("superseded_by"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
        }
    }
    crate::graph_store::apply_defaults(&mut rows, true);
    rows.truncate(asked_count);
    if !filter.is_empty() || query.fields.is_some() || query.with_blockers {
        for row in &mut rows {
            if row
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| node_claims.get(id))
                .is_some_and(|claim| claim.work)
                && row.get("locked_by").is_some_and(|value| !value.is_null())
                && row
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|status| matches!(status, "idea" | "ready"))
            {
                row["status"] = Value::String("in_progress".into());
            }
        }
    }
    if !filter.is_empty() {
        rows.retain(|row| match model::Node::from_json(row) {
            Ok(node) => api::filter_matches(&node, filter),
            Err(_) => filter.only_ids(),
        });
    }
    if !keep_malformed.unwrap_or(false) {
        rows.retain(Value::is_object);
    }
    if let Some(fields) = &query.fields {
        for row in &mut rows {
            if let Some(object) = row.as_object_mut() {
                object.retain(|key, _| fields.contains(key));
            }
        }
    }
    Ok(rows)
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
    let connection = read_connection(graph)?;
    nodes::versions(&connection, ids)
}

pub fn version(graph: &Path) -> Result<String, String> {
    let connection = read_connection(graph)?;
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

    /// A store stamped before the identity columns opens: the stamped fast
    /// path runs the identity migration, so the first reader of
    /// agent_sessions.fno_id meets the column instead of dying on it (the
    /// merge-refusal shape the fleet hit on stores minted before the
    /// identity columns landed).
    #[test]
    fn a_stamped_store_without_identity_columns_opens_and_gains_them() {
        let (_dir, graph) = fixture("identity-stamped.json");
        open(&graph).unwrap();
        let db = database_path(&graph);
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "DROP INDEX IF EXISTS agent_sessions_fno_id;
                 ALTER TABLE agent_sessions DROP COLUMN fno_id;
                 ALTER TABLE agent_sessions DROP COLUMN display_name;
                 ALTER TABLE agent_sessions DROP COLUMN links;",
            )
            .unwrap();
        }
        open(&graph).unwrap();
        let conn = Connection::open(&db).unwrap();
        let mut stmt = conn
            .prepare("SELECT name FROM pragma_table_info('agent_sessions')")
            .unwrap();
        let columns: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        for column in ["fno_id", "display_name", "links"] {
            assert!(
                columns.iter().any(|name| name == column),
                "the stamped store opened without {column}"
            );
        }
    }

    /// A first write that lands between an opener's unlocked row count and
    /// its empty-store stamp keeps its version. The stamp used to reset it
    /// to the empty hash, so the next writer's fence passed and its publish
    /// deleted the write.
    #[test]
    fn an_opener_never_restamps_the_empty_version_over_a_first_write() {
        // The AC14 resolution ladder rides the same boundary: an unmigrated
        // root answers a db-spelled anchor with the legacy store, a migrated
        // root answers an old-spelled anchor with the moved store. The
        // anchor kind probes the .db twin, so each arm seeds its own.
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        std::fs::write(root.join("graph.json"), "{}").unwrap();
        std::fs::write(root.join("graph.db"), b"SQLite format 3\0").unwrap();
        let db = database_path(&root.join("db").join("graph.json"));
        assert_eq!(db, root.join("graph.db"), "unmigrated: legacy store");
        std::fs::create_dir_all(root.join("db")).unwrap();
        std::fs::write(root.join("db").join("graph.json"), "{}").unwrap();
        std::fs::write(root.join("db").join("graph.db"), b"SQLite format 3\0").unwrap();
        let db = database_path(&root.join("graph.json"));
        assert_eq!(
            db,
            root.join("db").join("graph.db"),
            "migrated: moved store"
        );
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
                 "sessions": [{"phase": "execute", "harness": "claude",
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
    fn backlog_schema_import_keeps_fixtures_working_and_reads_pass_a_writer() {
        let dir = TempDir::new().unwrap();
        let graph = two_node_graph(&dir);
        let entries = read_entries(&graph).unwrap();
        assert_eq!(entries.len(), 2, "both rows imported");
        let live = api::nodes(
            &api::Store::new(&graph),
            &api::NodeFilter {
                state_type: Some("open".into()),
                ..Default::default()
            },
            &api::Page::default(),
        )
        .unwrap();
        assert_eq!(live.nodes.len(), 2, "open reads keep live rows");

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

        let started = std::time::Instant::now();
        connection.execute_batch("BEGIN IMMEDIATE;").unwrap();
        let entries = read_entries(&graph).unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(!entries.is_empty());
        connection.execute_batch("ROLLBACK;").unwrap();

        // A seed landing on an already-stamped EMPTY store parks blob rows
        // past the one-time setup; each open path must still fold them.
        let row_sql = "CREATE TABLE entries (
                           id TEXT PRIMARY KEY, ordinal INTEGER NOT NULL, row TEXT NOT NULL);
                       INSERT INTO entries VALUES ('ab-late', 0, '{\"id\": \"ab-late\",
                           \"slug\": \"late\", \"title\": \"Late\", \"type\": \"feature\",
                           \"status\": \"ready\", \"priority\": \"p2\", \"domain\": \"code\",
                           \"created_at\": \"2026-09-01T00:00:00+00:00\"}');";
        let via_write = dir.path().join("fold-on-write.json");
        drop(open(&via_write).unwrap());
        open(&via_write).unwrap().execute_batch(row_sql).unwrap();
        drop(open(&via_write).unwrap());
        assert_eq!(
            read_entries(&via_write).unwrap().len(),
            1,
            "a write open folds the parked rows"
        );
        let via_read = dir.path().join("fold-on-read.json");
        drop(open(&via_read).unwrap());
        open(&via_read).unwrap().execute_batch(row_sql).unwrap();
        assert_eq!(
            read_entries(&via_read).unwrap().len(),
            1,
            "a read folds the parked rows"
        );

        drop(connection);
        let db = database_path(&graph);
        drop(open(&db).unwrap());
        assert!(
            db.exists(),
            "an anchor naming the database never retires it"
        );
        assert_eq!(
            read_entries(&graph).unwrap().len(),
            2,
            "the store still answers"
        );

        let _env_lock = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _claims_root =
            crate::claims::EnvVarGuard::set("FNO_CLAIMS_ROOT", dir.path().to_str().unwrap());
        let query_graph = dir.path().join("query.json");
        let fixture = serde_json::json!([
            {"id":"q-done", "title":"Done", "slug":"closed", "status":"done", "completed_at":"2026-09-11T00:00:00Z", "parent":"q-live"},
            {"id":"q-live", "title":"Live", "slug":"live", "status":"ready", "project":"fno", "blocked_by":["q-old"], "tags":["a"], "sessions":[{"phase":"execute","harness":"codex","session_id":"test-session"}]},
            {"id":"q-old", "title":"Old", "slug":"old", "status":"superseded", "superseded_by":"q-done"},
            {"id":"q-archived", "title":"Archived", "slug":"archived", "status":"ready", "archived_at":"2026-09-11T00:00:00Z"},
            {"id":"q-deferred", "title":"Deferred", "slug":"deferred", "status":"deferred"},
            {"id":"q-claimed", "title":"Claimed", "slug":"claimed", "status":"claimed"},
            {"id":"q-raw", "title": 4, "status":"legacy-unknown", "slug":"raw", "parent":"q-live"}
        ]);
        crate::graph_store::seed_rows(&query_graph, fixture.as_array().unwrap()).unwrap();
        let full_raw = read_entries(&query_graph).unwrap();
        assert_eq!(
            read_entries_where(&query_graph, &RowQuery::default()).unwrap(),
            full_raw
        );
        let mut full = full_raw.clone();
        crate::graph_store::apply_defaults(&mut full, false);
        assert_eq!(crate::graph_store::read_rows(&query_graph).unwrap(), full);
        let query = RowQuery {
            filter: api::NodeFilter {
                state_type: Some("open".into()),
                ..Default::default()
            },
            include_archived: false,
            with_blockers: true,
            ..Default::default()
        };
        let narrowed = crate::graph_store::read_rows_where(&query_graph, &query).unwrap();
        let expected: Vec<Value> = full
            .iter()
            .filter(|row| {
                row["archived_at"].is_null()
                    && matches!(row["id"].as_str(), Some("q-live" | "q-claimed"))
            })
            .cloned()
            .collect();
        assert_eq!(
            narrowed, expected,
            "closed children and hidden successor preserve row parity"
        );
        let ordinals = row_ordinals(&query_graph).unwrap();
        let page = api::Page {
            first: Some(1),
            ..Default::default()
        };
        let old_page = api::nodes_in(&full, &query.filter, &page);
        let new_page = api::nodes_in_with_ordinals(&narrowed, &query.filter, &page, &ordinals);
        assert_eq!(new_page.page_info.end_cursor, old_page.page_info.end_cursor);
        let next_page = api::Page {
            after: new_page.page_info.end_cursor,
            ..page
        };
        let next = api::nodes(&api::Store::new(&query_graph), &query.filter, &next_page).unwrap();
        assert_eq!(next.nodes[0].id, "q-claimed");
        for (filter, wanted) in [
            (
                api::NodeFilter {
                    id_in: Some(vec!["LIVE".into()]),
                    ..Default::default()
                },
                "q-live",
            ),
            (
                api::NodeFilter {
                    project: Some("fno".into()),
                    ..Default::default()
                },
                "q-live",
            ),
            (
                api::NodeFilter {
                    parent: Some("q-live".into()),
                    ..Default::default()
                },
                "q-done",
            ),
            (
                api::NodeFilter {
                    label: Some("a".into()),
                    ..Default::default()
                },
                "q-live",
            ),
            (
                api::NodeFilter {
                    session_id: Some("test-session".into()),
                    ..Default::default()
                },
                "q-live",
            ),
            (
                api::NodeFilter {
                    status_in: Some(vec!["in_progress".into()]),
                    ..Default::default()
                },
                "q-claimed",
            ),
            (
                api::NodeFilter {
                    id_in: Some(vec!["raw".into()]),
                    ..Default::default()
                },
                "q-raw",
            ),
        ] {
            let rows = crate::graph_store::read_rows_where(
                &query_graph,
                &RowQuery {
                    filter,
                    fields: Some(vec!["id".into()]),
                    with_blockers: true,
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(rows, vec![serde_json::json!({"id":wanted})]);
        }

        let claim_options = crate::claims::AcquireOpts {
            root: Some(dir.path().into()),
            events_dir: Some(dir.path().into()),
            ttl_ms: Some(60_000),
            ..Default::default()
        };
        crate::claim_store::acquire_db("node:q-live", "test-worker", &claim_options).unwrap();
        crate::claim_store::acquire_db("node:q-archived", "blueprint-session:test", &claim_options)
            .unwrap();
        let planning = crate::graph_store::read_rows_where(
            &query_graph,
            &RowQuery {
                filter: api::NodeFilter {
                    id_in: Some(vec!["q-archived".into()]),
                    ..Default::default()
                },
                fields: Some(vec!["status".into()]),
                with_blockers: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            planning[0]["status"], "ready",
            "a planning claim does not serve work status"
        );
        let claimed_query = RowQuery {
            filter: api::NodeFilter {
                status_in: Some(vec!["in_progress".into()]),
                claimed: Some(true),
                ..Default::default()
            },
            fields: Some(vec!["id".into(), "status".into()]),
            with_blockers: true,
            ..Default::default()
        };
        assert_eq!(
            crate::graph_store::read_rows_where(&query_graph, &claimed_query).unwrap(),
            vec![serde_json::json!({"id":"q-live", "status":"in_progress"})]
        );
        crate::claim_store::release_db(
            "node:q-live",
            "test-worker",
            Some(dir.path()),
            Some(dir.path()),
        )
        .unwrap();
        crate::claim_store::release_db(
            "node:q-archived",
            "blueprint-session:test",
            Some(dir.path()),
            Some(dir.path()),
        )
        .unwrap();
        let mut projected = query.clone();
        projected.fields = Some(vec!["id".into(), "status".into()]);
        assert_eq!(
            crate::graph_store::read_rows_where(&query_graph, &projected).unwrap(),
            vec![
                serde_json::json!({"id":"q-live","status":"ready"}),
                serde_json::json!({"id":"q-claimed","status":"in_progress"})
            ]
        );
        assert!(crate::graph_store::read_rows_where_strict(
            &dir.path().join("missing.db"),
            &projected
        )
        .is_err());
        let connection = open(&query_graph).unwrap();
        connection
            .execute(
                "UPDATE nodes SET completed_at = NULL, status = 'ready' WHERE id = 'q-done'",
                [],
            )
            .unwrap();
        let blocked_query = RowQuery {
            filter: api::NodeFilter {
                status_in: Some(vec!["blocked".into()]),
                ..Default::default()
            },
            fields: Some(vec!["id".into()]),
            with_blockers: true,
            ..Default::default()
        };
        assert_eq!(
            crate::graph_store::read_rows_where(&query_graph, &blocked_query).unwrap(),
            vec![serde_json::json!({"id":"q-live"})]
        );

        let connection = open(&query_graph).unwrap();
        connection.execute("UPDATE nodes SET superseded_by = 'q-old', completed_at = NULL, status = 'superseded' WHERE id = 'q-done'", []).unwrap();
        let rows = crate::graph_store::read_rows_where(&query_graph, &projected).unwrap();
        assert_eq!(
            rows[0]["status"], "blocked",
            "cycles remain unknown dependencies"
        );
        connection
            .execute("DELETE FROM nodes WHERE id = 'q-old'", [])
            .unwrap();
        assert_eq!(
            crate::graph_store::read_rows_where(&query_graph, &projected).unwrap()[0]["status"],
            "blocked"
        );

        let mut raw_blocker = serde_json::json!({"id":"q-old", "title":4, "status":"done", "completed_at":"2026-09-11T00:00:00Z"});
        nodes::save_raw(&connection, "q-old", 30, &raw_blocker).unwrap();
        assert_eq!(
            crate::graph_store::read_rows_where(&query_graph, &projected).unwrap()[0]["status"],
            "ready",
            "raw-carried completed blocker releases a dependent"
        );
        raw_blocker["completed_at"] = Value::Null;
        nodes::save_raw(&connection, "q-old", 30, &raw_blocker).unwrap();
        assert_eq!(
            crate::graph_store::read_rows_where(&query_graph, &projected).unwrap()[0]["status"],
            "blocked"
        );
        nodes::delete_raw(&connection, "q-old").unwrap();
        save_aggregate(&connection, &model::Node::from_json(&serde_json::json!({
            "id":"q-old", "slug":"old", "title":"Old", "status":"done", "completed_at":"2026-09-11T00:00:00Z"
        })).unwrap()).unwrap();
        connection.execute_batch("DROP TABLE sessions; DROP TABLE comments; DROP TABLE encounters; DROP TABLE findings; DROP TABLE node_costs; DROP TABLE node_dispatch; DROP TABLE node_provenance; DROP TABLE pull_requests;").unwrap();
        assert!(
            crate::graph_store::read_rows_where(&query_graph, &projected).is_ok(),
            "unrequested aggregates are not read"
        );
        assert_eq!(
            crate::graph_store::read_rows_where(&query_graph, &projected).unwrap()[0]["status"],
            "ready"
        );
        assert!(
            crate::graph_store::read_rows(&query_graph).is_err(),
            "whole reads still surface aggregate failures"
        );
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
                     \"created_at\": \"2026-09-01T00:00:00+00:00\"}');
                 DELETE FROM graph_meta WHERE key = 'open_setup_version';",
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
        let _env_lock = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
    fn single_row_write_passes_a_legacy_empty_field_on_another_row() {
        let _env_lock = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let spaces = tempfile::TempDir::new().unwrap();
        declare_test_roots(spaces.path());
        let (_dir, graph) = seeded_sqlite_fixture();
        // Seed the trap the store itself can produce: a closed row holding
        // an empty details (stored as the description column). The write
        // routes through the table's owning module (the table-ownership
        // gate), like every write.
        {
            let connection = open(&graph).unwrap();
            let mut one = crate::backlog::nodes::load(&connection, "ab-one")
                .unwrap()
                .unwrap();
            one.status = crate::backlog::model::Status::parse("done").unwrap();
            one.description = Some(String::new());
            crate::backlog::nodes::save(&connection, &one).unwrap();
        }
        // A write touching only the OTHER row succeeds.
        let ok = mutate_single_row(&graph, "comment_create", |rows| {
            for row in rows.iter_mut() {
                if entry_id_from(row) == Some("ab-two") {
                    row.as_object_mut()
                        .unwrap()
                        .insert("progress_notes".into(), serde_json::json!([{"body": "hi"}]));
                }
            }
            Ok(true)
        })
        .unwrap();
        assert!(ok);
        // An empty value the write introduces still refuses, naming that row.
        let err = mutate_single_row(&graph, "node_update", |rows| {
            for row in rows.iter_mut() {
                if entry_id_from(row) == Some("ab-two") {
                    row.as_object_mut()
                        .unwrap()
                        .insert("title".into(), serde_json::json!(""));
                }
            }
            Ok(true)
        })
        .unwrap_err();
        assert!(err.contains("ab-two"), "{err}");
        // The legacy row still reads details "".
        let rows = export_rows(&open(&graph).unwrap()).unwrap();
        let one = rows
            .iter()
            .find(|r| entry_id_from(r) == Some("ab-one"))
            .unwrap();
        assert_eq!(
            one.get("details"),
            Some(&serde_json::Value::String(String::new()))
        );
    }

    #[test]
    fn single_row_concurrent_writes_both_persist_with_positive_readback() {
        let _env_lock = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
        let _env_lock = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
        let _env_lock = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
