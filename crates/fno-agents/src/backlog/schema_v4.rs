//! Schema 4: every table carries `created_at` and `updated_at`, every
//! timestamp column refuses a value that is not UTC ISO-8601, every harness,
//! model and session id points at an entity row, and relations point at real
//! nodes. The DDL lives with each owning module; this file holds the shared
//! column helpers and the one-way schema-3 migration that runs inside
//! `backlog::open`. docs/architecture/graph-db-schema.md is the reference.

use rusqlite::{Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The one spelling the store writes for its own timestamps.
pub const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%fZ','now')";

/// The named CHECK a timestamp column carries, so a refusal names the column.
pub fn iso(table: &str, column: &str) -> String {
    format!(
        "CONSTRAINT {table}_{column}_iso CHECK ({column} IS NULL OR (datetime({column}) IS NOT NULL
           AND ({column} GLOB '*Z' OR {column} GLOB '*+00:00')))"
    )
}

/// The store-owned created_at and updated_at columns with their CHECKs.
/// Appended after a table's last column definition.
pub fn stamps(table: &str) -> String {
    format!(
        ",
  created_at TEXT NOT NULL DEFAULT ({NOW}),
  updated_at TEXT NOT NULL DEFAULT ({NOW}),
  {},
  {}",
        iso(table, "created_at"),
        iso(table, "updated_at")
    )
}

/// The store-owned updated_at column alone, for a table whose created_at
/// is a wire field it already carries.
pub fn updated(table: &str) -> String {
    format!(
        ",
  updated_at TEXT NOT NULL DEFAULT ({NOW}),
  {}",
        iso(table, "updated_at")
    )
}

/// The updated_at trigger: a write that did not set updated_at itself gets
/// the current time. `recursive_triggers` is off, so the inner UPDATE does
/// not re-fire it.
pub fn touch(table: &str) -> String {
    format!(
        "CREATE TRIGGER IF NOT EXISTS {table}_touch AFTER UPDATE ON {table}
         WHEN NEW.updated_at IS OLD.updated_at BEGIN
           UPDATE {table} SET updated_at = {NOW} WHERE rowid = NEW.rowid;
         END;\n"
    )
}

/// The node_claims mirror table exactly as schema 4 shipped it. The v4
/// migration's copy step still writes it; the mirror itself is retired and
/// nodes::ensure_table drops it on the open right after the migration.
pub fn v4_node_claims_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS node_claims (
  node_id TEXT PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
  locked_by TEXT, harness TEXT REFERENCES harnesses(id),
  harness_session TEXT REFERENCES agent_sessions(id), created_at TEXT{},
  {}
);",
        updated("node_claims"),
        iso("node_claims", "created_at")
    )
}

/// A value in the one form a UTC ISO-8601 CHECK accepts: `Z` or `+00:00`
/// and a parse. The model's legacy-key fold uses it, so a folded value
/// always passes the column's CHECK.
pub fn is_utc_iso(value: &str) -> bool {
    (value.ends_with('Z') || value.ends_with("+00:00"))
        && chrono::DateTime::parse_from_rfc3339(value).is_ok()
}

/// SQL for "this expression is a UTC ISO-8601 string", the CHECK's rule.
pub(crate) fn iso_sql(expr: &str) -> String {
    format!("(datetime({expr}) IS NOT NULL AND ({expr} GLOB '*Z' OR {expr} GLOB '*+00:00'))")
}

/// SQL normalizing a timestamp expression to the store's own form, NULL
/// when it does not parse.
pub(crate) fn norm_sql(expr: &str) -> String {
    format!("strftime('%Y-%m-%dT%H:%M:%fZ', {expr})")
}

/// The schema-3 tables the migration rebuilds, in copy order.
const V3_TABLES: &[&str] = &[
    "graph_meta",
    "nodes",
    "nodes_raw",
    "node_claims",
    "node_dispatch",
    "node_provenance",
    "supersessions",
    "sessions",
    "comments",
    "encounters",
    "pull_requests",
    "relations",
    "decisions",
    "node_decisions",
    "findings",
];

pub(crate) fn table_exists(connection: &Connection, name: &str) -> Result<bool, String> {
    connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [name],
            |_| Ok(()),
        )
        .optional()
        .map(|found| found.is_some())
        .map_err(|error| error.to_string())
}

/// A store still in the schema-3 shape: a nodes table with no updated_at.
/// Shape, not the version string, decides, so no backend or version path
/// can skip the rebuild.
pub fn needs_migration(connection: &Connection) -> Result<bool, String> {
    if !table_exists(connection, "nodes")? {
        return Ok(false);
    }
    let has: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('nodes') WHERE name = 'updated_at'",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    Ok(has == 0)
}

/// Where the pre-migration snapshot lands. The name avoids the `graph.db.`
/// prefix, so the hourly rotation never deletes it.
fn snapshot_path(graph: &Path) -> Result<PathBuf, String> {
    let parent = graph
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", graph.display()))?;
    let dir = parent.join("backups");
    std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    Ok(dir.join(format!(
        "graph-pre-v4.db.{}",
        crate::graph_store::backup_stamp()
    )))
}

/// Counts the migration writes into `graph_meta.schema_v4_report`.
#[derive(Default, serde::Serialize)]
pub struct Report {
    pub snapshot: String,
    pub entities_created: i64,
    pub legacy_session_ids: i64,
    pub harness_conflicts: i64,
    pub quoted_at_fixed: i64,
    pub at_kept_in_extras: i64,
    pub relations_parked: i64,
    pub relations_dropped: i64,
    pub provenance_promoted: i64,
    pub cost_nodes_promoted: i64,
    pub cost_nodes_kept_in_extras: i64,
}

/// The one-way schema-3 to schema-4 rebuild. Runs in `backlog::open`
/// before the tables are ensured. Exactly one opener migrates: the shape is
/// re-read under the IMMEDIATE lock, and a loser returns with no snapshot
/// and no rebuild. A foreign_key_check failure rolls everything back and
/// names the row, the snapshot, and the rollback steps.
pub fn migrate_if_needed(connection: &mut Connection, graph: &Path) -> Result<(), String> {
    if !needs_migration(connection)? {
        return Ok(());
    }
    // foreign_keys cannot change inside a transaction, and the legacy
    // rename keeps the schema-3 tables' FK text pointing at the names the
    // new tables take.
    connection
        .execute_batch("PRAGMA foreign_keys=OFF; PRAGMA legacy_alter_table=ON;")
        .map_err(|error| error.to_string())?;
    // A second opener waits out the whole rebuild instead of failing busy.
    connection
        .busy_timeout(Duration::from_secs(120))
        .map_err(|error| error.to_string())?;
    let outcome = migrate_locked(connection, graph);
    let restored = connection
        .execute_batch("PRAGMA foreign_keys=ON; PRAGMA legacy_alter_table=OFF;")
        .map_err(|error| error.to_string())
        .and_then(|()| {
            connection
                .busy_timeout(Duration::from_secs(5))
                .map_err(|error| error.to_string())
        });
    outcome.and(restored)
}

fn migrate_locked(connection: &mut Connection, graph: &Path) -> Result<(), String> {
    let transaction = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    if !needs_migration(&transaction)? {
        return Ok(());
    }
    let snapshot = snapshot_path(graph)?;
    // VACUUM INTO cannot run inside a transaction, so a second read-only
    // connection writes the snapshot of the committed schema-3 state while
    // this one holds the write lock.
    {
        let reader = Connection::open_with_flags(
            super::database_path(graph),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|error| error.to_string())?;
        reader
            .execute("VACUUM INTO ?1", [snapshot.display().to_string()])
            .map_err(|error| format!("schema v4 snapshot {}: {error}", snapshot.display()))?;
    }
    let mut report = Report {
        snapshot: snapshot.display().to_string(),
        ..Report::default()
    };
    rebuild(&transaction, &mut report).map_err(|error| failure(&snapshot, &error))?;
    let violations = foreign_key_violations(&transaction)?;
    if !violations.is_empty() {
        return Err(failure(
            &snapshot,
            &format!("foreign_key_check: {}", violations.join("; ")),
        ));
    }
    let current: i64 = super::meta(&transaction, "schema_version")?
        .and_then(|raw| raw.parse::<i64>().ok())
        .unwrap_or(2);
    // A schema-2 stamp stays for the json-backend rebuild that follows in
    // the same open; it stamps schema 4 itself.
    if current >= 3 {
        super::stamp_meta(&transaction, "schema_version", super::SCHEMA_VERSION)?;
    }
    let report = serde_json::to_string(&report).map_err(|error| error.to_string())?;
    super::stamp_meta(&transaction, "schema_v4_report", &report)?;
    transaction.commit().map_err(|error| error.to_string())
}

fn failure(snapshot: &Path, error: &str) -> String {
    format!(
        "schema v4 migration refused and rolled back: {error}. Nothing changed; the \
         pre-migration snapshot is {}. Fix what the error names with the previous \
         release, then retry. See docs/architecture/graph-db-schema.md for the rollback steps.",
        snapshot.display()
    )
}

fn foreign_key_violations(connection: &Connection) -> Result<Vec<String>, String> {
    let mut statement = connection
        .prepare("PRAGMA foreign_key_check")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok(format!(
                "{} rowid {} -> {}",
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?
                    .map_or("?".to_string(), |id| id.to_string()),
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|error| error.to_string())?);
        if out.len() >= 10 {
            break;
        }
    }
    Ok(out)
}

fn rebuild(connection: &Connection, report: &mut Report) -> Result<(), String> {
    // A schema-2 store has no extras columns yet; the copies read them.
    for add in [
        super::sessions::migrate_add_extras,
        super::comments::migrate_add_extras,
        super::encounters::migrate_add_extras,
        super::pull_requests::migrate_add_extras,
    ] {
        add(connection)?;
    }
    let present: Vec<&str> = V3_TABLES
        .iter()
        .copied()
        .filter(|table| table_exists(connection, table).unwrap_or(false))
        .collect();
    // Indexes and triggers on the old tables keep their names through a
    // rename; drop them so the new tables can take the same names.
    let mut owned = Vec::new();
    {
        let mut statement = connection
            .prepare(
                "SELECT type, name, tbl_name FROM sqlite_master
                 WHERE type IN ('index', 'trigger') AND sql IS NOT NULL",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|error| error.to_string())?;
        for row in rows {
            owned.push(row.map_err(|error| error.to_string())?);
        }
    }
    for (kind, name, table) in owned {
        if present.contains(&table.as_str()) {
            connection
                .execute_batch(&format!(
                    "DROP {} IF EXISTS \"{name}\";",
                    kind.to_uppercase()
                ))
                .map_err(|error| error.to_string())?;
        }
    }
    for table in &present {
        connection
            .execute_batch(&format!("ALTER TABLE {table} RENAME TO {table}_v3;"))
            .map_err(|error| error.to_string())?;
    }
    // The new tables, with no triggers yet: the copies below must not
    // fire updated_at, entity or relation triggers.
    for ddl in [
        super::graph_meta_ddl(),
        super::entities::ddl(),
        super::nodes::ddl(),
        // The v4 migration owns this table's birth: the copy step below
        // still writes it, and nodes::ensure_table drops it right after
        // ensure on every open (the mirror retires under the migration,
        // never under an edited v4 step).
        v4_node_claims_ddl(),
        super::sessions::ddl(),
        super::comments::ddl(),
        super::encounters::ddl(),
        super::pull_requests::ddl(),
        super::relations::ddl(),
        super::decisions::ddl(),
        super::costs::ddl(),
        super::findings::ddl(),
    ] {
        connection
            .execute_batch(&ddl)
            .map_err(|error| error.to_string())?;
    }
    let (made, legacy, conflicts) = super::entities::backfill_from_v3(connection)?;
    report.entities_created = made;
    report.legacy_session_ids = legacy;
    report.harness_conflicts = conflicts;
    super::copy_graph_meta_from_v3(connection)?;
    report.provenance_promoted = super::nodes::copy_from_v3(connection)?;
    let (fixed, kept) = super::sessions::copy_from_v3(connection)?;
    report.quoted_at_fixed = fixed;
    report.at_kept_in_extras = kept;
    super::comments::copy_from_v3(connection)?;
    super::encounters::copy_from_v3(connection)?;
    super::pull_requests::copy_from_v3(connection)?;
    let (parked, dropped) = super::relations::copy_from_v3(connection)?;
    report.relations_parked = parked;
    report.relations_dropped = dropped;
    if present.contains(&"decisions") {
        super::decisions::copy_from_v3(connection, present.contains(&"node_decisions"))?;
    }
    if present.contains(&"findings") {
        super::findings::copy_from_v3(connection)?;
    }
    let (promoted, kept) = super::costs::promote_from_extras(connection)?;
    super::nodes::strip_extras_key(connection, "cost_sessions", &promoted)?;
    report.cost_nodes_promoted = promoted.len() as i64;
    report.cost_nodes_kept_in_extras = kept;
    for table in &present {
        connection
            .execute_batch(&format!("DROP TABLE {table}_v3;"))
            .map_err(|error| error.to_string())?;
    }
    super::ensure_triggers(connection)?;
    super::search::ensure_table(connection)?;
    super::search::rebuild(connection)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A seeded schema-3 store. The SQL lives outside src/ so the
    /// table-ownership scan never reads its raw writes.
    const V3_FIXTURE: &str = include_str!("../../tests/fixtures/graph_db_schema_v3.sql");

    fn v3_store() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let graph = dir.path().join("graph.json");
        std::fs::write(&graph, b"{\"entries\": []}").unwrap();
        let connection = Connection::open(super::super::database_path(&graph)).unwrap();
        connection.execute_batch(V3_FIXTURE).unwrap();
        drop(connection);
        (dir, graph)
    }

    fn rows(connection: &Connection, sql: &str) -> Vec<String> {
        let mut statement = connection.prepare(sql).unwrap();
        let found = statement.query_map([], |row| row.get(0)).unwrap();
        found.collect::<Result<Vec<String>, _>>().unwrap()
    }

    fn tables(connection: &Connection) -> Vec<String> {
        rows(
            connection,
            "SELECT name FROM sqlite_master WHERE type = 'table'
             AND name NOT LIKE 'sqlite_%' AND name NOT LIKE 'nodes_fts%' ORDER BY name",
        )
    }

    #[test]
    fn a_schema_3_store_migrates_to_4_with_its_wire_json_kept() {
        // The claim projection rides every read, so pin an empty claims root:
        // the served word must not depend on the operator's live claims.
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let claims_root = tempfile::TempDir::new().unwrap();
        std::env::set_var("FNO_CLAIMS_ROOT", claims_root.path());
        let (dir, graph) = v3_store();
        let connection = crate::backlog::open(&graph).unwrap();
        assert_eq!(
            crate::backlog::meta(&connection, "schema_version").unwrap(),
            Some("4".into())
        );
        let violations = foreign_key_violations(&connection).unwrap();
        assert!(violations.is_empty(), "{violations:?}");
        for table in tables(&connection) {
            let columns = rows(
                &connection,
                &format!("SELECT name FROM pragma_table_info('{table}')"),
            );
            for stamp in ["created_at", "updated_at"] {
                assert!(
                    columns.contains(&stamp.to_string()),
                    "{table} has no {stamp}"
                );
            }
        }
        let snapshots: Vec<_> = std::fs::read_dir(dir.path().join("backups"))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("graph-pre-v4.db.")
            })
            .collect();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(
            rows(
                &connection,
                "SELECT finding_id || ' ' || (resolved_at IS NOT NULL) || ' '
                        || (SELECT harness_id FROM agent_sessions WHERE id = 's-9')
                 FROM findings"
            ),
            vec!["f-1 1 annotate-import"],
            "a finding migrates with its entities, and a bad resolved stamp stays resolved"
        );
        let entries = crate::backlog::export_rows(&connection).unwrap();
        let a = entries.iter().find(|row| row["id"] == "x-a").unwrap();
        // The renamed and folded keys: the quoted `at` became started_at,
        // the encounter's ts is created_at, and the lock stays locked_at.
        assert_eq!(a["sessions"][0]["started_at"], "2026-07-19T16:43:13Z");
        assert!(a["sessions"][0].get("at").is_none());
        assert_eq!(
            a["encounters"][0]["created_at"],
            "2026-09-11T03:00:00+00:00"
        );
        assert!(a["encounters"][0].get("ts").is_none());
        // The retired mirror's stamps stay in storage (the fold minted the
        // claude-shaped session id and stamped the node's session_id), while
        // the served word projects the holder store: no live claim projects
        // no holder and no lock stamp.
        assert_eq!(
            rows(&connection, "SELECT session_id FROM nodes WHERE id = 'x-a'"),
            vec!["20260911T051456Z-cl67883-05ec5f".to_string()]
        );
        assert_eq!(a["locked_at"], serde_json::Value::Null);
        assert_eq!(a["blocked_by"], serde_json::json!(["x-9999"]));
        assert_eq!(a["request_origin"], "operator_request");
        assert_eq!(a["origin_evidence"], "said so");
        assert_eq!(
            a["cost_sessions"],
            serde_json::json!([
                {"session_id": "s-1", "cost_usd": 1.5, "timestamp": "2026-09-11T01:00:00+00:00"},
                {"session_id": "s-2", "cost_usd": 0.5}
            ])
        );
        assert_eq!(a["session_id"], serde_json::Value::Null);
        let b = entries.iter().find(|row| row["id"] == "x-b").unwrap();
        assert_eq!(
            b["progress_notes"][1],
            serde_json::json!({"ts": "T1", "text": "odd stamp"}),
            "a note stamp that is not UTC migrates into extras"
        );
        assert_eq!(
            b["cost_sessions"][0]["timestamp"], "2026-08-21T13:10:35.335800",
            "a naive cost timestamp stays in extras"
        );
        assert_eq!(
            rows(&connection, "SELECT node_id FROM node_costs ORDER BY seq"),
            vec!["x-a", "x-a"]
        );
        assert_eq!(
            rows(&connection, "SELECT node_id FROM node_decisions"),
            vec!["x-a"]
        );
        assert_eq!(
            rows(
                &connection,
                "SELECT related_node_id FROM relations_unresolved"
            ),
            vec!["x-a"]
        );
        assert_eq!(
            rows(
                &connection,
                "SELECT id FROM agent_sessions WHERE harness_id IS NULL ORDER BY id"
            ),
            vec![
                "20260911T051456Z-cl67883-05ec5f".to_string(),
                "s-3".into(),
                "s-8".into()
            ]
        );
        let report: serde_json::Value = serde_json::from_str(
            &crate::backlog::meta(&connection, "schema_v4_report")
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(report["quoted_at_fixed"], 1);
        assert_eq!(report["relations_parked"], 1);
        assert_eq!(report["relations_dropped"], 1);
        assert_eq!(report["cost_nodes_promoted"], 1);
        assert_eq!(report["cost_nodes_kept_in_extras"], 1);
        assert_eq!(
            crate::backlog::search::search(&crate::backlog::api::Store::new(&graph), "Alpha", None)
                .unwrap()
                .first()
                .map(String::as_str),
            Some("x-a"),
            "the fts index was rebuilt over the new rowids"
        );
    }

    /// A schema-2 child table has no extras column. The migration adds it
    /// before the copy, and the old rows load with empty extras.
    #[test]
    fn a_child_table_without_extras_migrates_with_its_rows() {
        let (_dir, graph) = v3_store();
        Connection::open(super::super::database_path(&graph))
            .unwrap()
            .execute_batch("ALTER TABLE comments DROP COLUMN extras;")
            .unwrap();
        let connection = crate::backlog::open(&graph).unwrap();
        let comments = crate::backlog::comments::load(&connection, "x-b").unwrap();
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].body.as_deref(), Some("kept"));
        assert!(comments[0].extras.is_empty());
        assert_eq!(comments[1].extras["ts"], "T1");
    }

    #[test]
    fn a_bad_timestamp_is_refused_by_its_constraint_name() {
        let (_dir, graph) = v3_store();
        let connection = crate::backlog::open(&graph).unwrap();
        let mut node = crate::backlog::nodes::load(&connection, "x-b")
            .unwrap()
            .unwrap();
        node.completed_at = Some("yesterday".into());
        let before = crate::backlog::export_rows(&connection).unwrap();
        let error = crate::backlog::save_aggregate(&connection, &node).unwrap_err();
        assert!(error.contains("nodes_completed_at_iso"), "{error}");
        assert_eq!(crate::backlog::export_rows(&connection).unwrap(), before);
    }

    #[test]
    fn two_openers_migrate_once() {
        let (dir, graph) = v3_store();
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let path = graph.clone();
                std::thread::spawn(move || crate::backlog::open(&path).map(|_| ()))
            })
            .collect();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }
        let snapshots = std::fs::read_dir(dir.path().join("backups"))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("graph-pre-v4.db.")
            })
            .count();
        assert_eq!(snapshots, 1, "exactly one opener migrated");
    }

    #[test]
    fn a_parked_relation_resolves_when_its_node_arrives() {
        let (_dir, graph) = v3_store();
        let connection = crate::backlog::open(&graph).unwrap();
        let node = crate::backlog::model::Node::from_json(&serde_json::json!({
            "id": "x-9999", "slug": "late", "title": "Late", "type": "feature",
            "status": "ready", "priority": "p2", "domain": "code",
            "created_at": "2026-09-20T00:00:00Z",
        }))
        .unwrap();
        crate::backlog::save_aggregate(&connection, &node).unwrap();
        assert!(rows(&connection, "SELECT node_id FROM relations_unresolved").is_empty());
        assert_eq!(
            rows(
                &connection,
                "SELECT node_id FROM relations WHERE related_node_id = 'x-a'"
            ),
            vec!["x-9999"]
        );
        assert!(foreign_key_violations(&connection).unwrap().is_empty());
        let a = crate::backlog::nodes::load(&connection, "x-a")
            .unwrap()
            .unwrap();
        assert_eq!(a.relations.blocked_by, Some(vec!["x-9999".to_string()]));
    }

    #[test]
    fn a_deleted_node_parks_the_edges_that_name_it() {
        let (_dir, graph) = v3_store();
        let connection = crate::backlog::open(&graph).unwrap();
        let node = crate::backlog::model::Node::from_json(&serde_json::json!({
            "id": "x-c", "slug": "c", "title": "C", "type": "feature",
            "status": "ready", "priority": "p2", "domain": "code",
            "blocked_by": ["x-b"],
        }))
        .unwrap();
        crate::backlog::save_aggregate(&connection, &node).unwrap();
        crate::backlog::delete_aggregate(&connection, "x-b").unwrap();
        let c = crate::backlog::nodes::load(&connection, "x-c")
            .unwrap()
            .unwrap();
        assert_eq!(c.relations.blocked_by, Some(vec!["x-b".to_string()]));
        assert!(foreign_key_violations(&connection).unwrap().is_empty());
    }

    #[test]
    fn the_write_connection_enforces_foreign_keys() {
        let dir = TempDir::new().unwrap();
        let graph = dir.path().join("graph.json");
        std::fs::write(&graph, b"{\"entries\": []}").unwrap();
        let connection = crate::backlog::open(&graph).unwrap();
        let on: i64 = connection
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap();
        assert_eq!(on, 1);
    }

    /// The live-store rehearsal (AC5-HP). `FNO_V4_REHEARSAL_DB` names a
    /// COPY of a schema-3 graph.db; `FNO_V4_REHEARSAL_BEFORE` names the
    /// `read` export the deployed binary made of that copy. The test copies
    /// the db again, migrates the copy, and compares every node's JSON
    /// after the wire renames.
    #[test]
    #[ignore]
    fn rehearsal_live_copy() {
        let source =
            PathBuf::from(std::env::var("FNO_V4_REHEARSAL_DB").expect("set FNO_V4_REHEARSAL_DB"));
        let before: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                std::env::var("FNO_V4_REHEARSAL_BEFORE").expect("set FNO_V4_REHEARSAL_BEFORE"),
            )
            .unwrap(),
        )
        .unwrap();
        let before = before
            .get("entries")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .expect("the before export holds entries");
        let dir = source.parent().unwrap().join("rehearsal");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let graph = dir.join("graph.json");
        std::fs::copy(&source, super::super::database_path(&graph)).unwrap();
        let started = std::time::Instant::now();
        let connection = crate::backlog::open(&graph).unwrap();
        println!("migration took {:?}", started.elapsed());
        assert_eq!(
            crate::backlog::meta(&connection, "schema_version").unwrap(),
            Some("4".into())
        );
        assert!(foreign_key_violations(&connection).unwrap().is_empty());
        let integrity: String = connection
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");
        let after = crate::backlog::export_rows(&connection).unwrap();
        assert_eq!(after.len(), before.len(), "node count");
        let renamed: Vec<serde_json::Value> = before.iter().map(apply_wire_renames).collect();
        let mut differ = Vec::new();
        for (old, new) in renamed.iter().zip(after.iter()) {
            if crate::backlog::canonical_row(old) != crate::backlog::canonical_row(new) {
                differ.push(old["id"].as_str().unwrap_or("?").to_string());
            }
        }
        assert!(
            differ.is_empty(),
            "{} node(s) changed JSON: {:?}",
            differ.len(),
            &differ[..differ.len().min(20)]
        );
        println!(
            "rehearsal: {} nodes export the same canonical JSON after the wire renames; report {}",
            after.len(),
            crate::backlog::meta(&connection, "schema_v4_report")
                .unwrap()
                .unwrap_or_default()
        );
    }

    /// The user's 2026-09-23 wire renames, applied to a schema-3 export: a
    /// session's `at`/`claimed_at` fold into started_at, an encounter's
    /// `ts` becomes created_at.
    fn apply_wire_renames(row: &serde_json::Value) -> serde_json::Value {
        let mut row = row.clone();
        if let Some(list) = row.get_mut("sessions").and_then(|v| v.as_array_mut()) {
            for item in list.iter_mut().filter_map(|v| v.as_object_mut()) {
                for legacy in ["at", "claimed_at"] {
                    let Some(value) = item.get(legacy).and_then(|v| v.as_str()) else {
                        continue;
                    };
                    if !is_utc_iso(value) {
                        continue;
                    }
                    let value = value.to_string();
                    item.remove(legacy);
                    item.entry("started_at".to_string())
                        .or_insert(serde_json::Value::String(value));
                }
            }
        }
        if let Some(list) = row.get_mut("encounters").and_then(|v| v.as_array_mut()) {
            for item in list.iter_mut().filter_map(|v| v.as_object_mut()) {
                if let Some(ts) = item.remove("ts") {
                    item.insert("created_at".to_string(), ts);
                }
            }
        }
        row
    }
}
