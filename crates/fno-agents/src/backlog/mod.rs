//! The backlog store: graph.db lifecycle (schema, import, sync, export),
//! the backend naming, and (wave 5) parity. Each aggregate's tables live in
//! its owning module and no other file writes them (ruling 4);
//! `TABLE_OWNERS` names the owners and the table_ownership test enforces
//! it. JSON stays authoritative through this group: graph.db is the
//! relational shadow, and every mutation writes only the changed nodes'
//! rows in one transaction.

pub mod comments;
pub mod encounters;
pub mod model;
pub mod nodes;
pub mod pull_requests;
pub mod relations;
pub mod sessions;

use crate::backlog::model::Node;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The schema stamp the import writes after the blob `entries` table is
/// dropped.
pub const SCHEMA_VERSION: &str = "2";

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

fn open(graph: &Path) -> Result<Connection, String> {
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
    Ok(connection)
}

/// The one-shot import: a db holding the blob `entries` table and no
/// `nodes` imports its rows into the owned tables, drops `entries`, and
/// stamps schema_version 2. A path with graph.json and no graph.db imports
/// on first open, which keeps the hundreds of test files that seed
/// graph.json fixtures working. A populated nodes table never re-imports.
fn import_if_needed(connection: &mut Connection, graph: &Path) -> Result<(), String> {
    let nodes_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0))
        .map_err(|error| error.to_string())?;
    if nodes_count > 0 {
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
        let mut node = Node::from_json(row).map_err(|error| format!("import: {error}"))?;
        node.ordinal = ordinal as i64;
        save_aggregate(&transaction, &node).map_err(|error| format!("import: {error}"))?;
    }
    if has_entries {
        transaction
            .execute("DROP TABLE entries", [])
            .map_err(|error| error.to_string())?;
    }
    stamp_version(&transaction, &content_version(&rows))?;
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

fn stamp_version(connection: &Connection, version: &str) -> Result<(), String> {
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

fn stamp_meta(connection: &Connection, key: &str, value: &str) -> Result<(), String> {
    connection
        .execute(
            "INSERT INTO graph_meta(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
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
    write_changed(&transaction, before, after)?;
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
    write_changed(&transaction, before, after)?;
    let version = content_version(after);
    stamp_version(&transaction, &version)?;
    transaction.commit().map_err(|error| error.to_string())?;
    Ok(version)
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
fn write_changed(connection: &Connection, before: &[Value], after: &[Value]) -> Result<(), String> {
    fn by_id(rows: &[Value]) -> std::collections::BTreeMap<String, String> {
        rows.iter()
            .filter(|row| row.is_object())
            .filter_map(|row| {
                crate::graph_store::entry_id(row)
                    .map(|id| (id.to_string(), crate::graph_store::to_python_json(row)))
            })
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
    for id in ids {
        let old = before_map.get(&id);
        let new = after_map.get(&id);
        if old == new {
            continue;
        }
        match new {
            Some(body) => {
                let row: Value = serde_json::from_str(body)
                    .map_err(|error| format!("changed row {id} is invalid JSON: {error}"))?;
                let mut node =
                    Node::from_json(&row).map_err(|error| format!("changed row {id}: {error}"))?;
                node.ordinal = ordinals.get(id.as_str()).copied().unwrap_or(0);
                save_aggregate(connection, &node)?;
            }
            None => delete_aggregate(connection, &id)?,
        }
    }
    Ok(())
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

fn meta(connection: &Connection, key: &str) -> Result<Option<String>, String> {
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
    crate::graph_store::create_backup(graph);
    crate::graph_store::write_atomic(graph, &crate::graph_store::serialize_graph_file(&entries))
        .map_err(|error| error.to_string())?;
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

pub fn export_if_due(graph: &Path, debounce: Duration) -> Result<bool, String> {
    let connection = open(graph)?;
    let current = meta(&connection, "version")?;
    let exported = meta(&connection, "exported_version")?;
    if current.is_none() || current == exported {
        return Ok(false);
    }
    let updated = meta(&connection, "updated_ms")?
        .and_then(|value| value.parse::<u128>().ok())
        .unwrap_or(0);
    if now_ms().saturating_sub(updated) < debounce.as_millis() {
        return Ok(false);
    }
    drop(connection);
    export_now(graph)?;
    Ok(true)
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
        out.insert(
            id.to_string(),
            crate::graph_store::to_python_json(&sorted_value(
                &crate::backlog::model::strip_nulls_value(row),
            )),
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
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
        // The probe is read-only: it never creates graph.db.
        assert!(!database_path(&graph).exists());
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
        for table in ["sessions", "relations"] {
            let rows: i64 = connection
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(rows, 0, "{table} holds no rows for the deleted node");
        }
    }
}
