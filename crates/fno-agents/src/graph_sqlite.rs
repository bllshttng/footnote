//! Row store shadowing the canonical graph export.

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub fn database_path(graph: &Path) -> PathBuf {
    graph.with_extension("db")
}

fn open(graph: &Path) -> Result<Connection, String> {
    let path = database_path(graph);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let connection = Connection::open(&path).map_err(|error| error.to_string())?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(|error| error.to_string())?;
    connection
        .execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS entries (
                 id TEXT PRIMARY KEY,
                 ordinal INTEGER NOT NULL,
                 row TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS graph_meta (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );",
        )
        .map_err(|error| error.to_string())?;
    Ok(connection)
}

fn rows_by_id(entries: &[Value]) -> BTreeMap<String, String> {
    entries
        .iter()
        .filter_map(|row| {
            crate::graph_store::entry_id(row).map(|id| {
                (
                    id.to_string(),
                    crate::graph_store::to_python_json(row),
                )
            })
        })
        .collect()
}

pub fn shadow_sync(
    graph: &Path,
    before: &[Value],
    after: &[Value],
    json_version: &str,
) -> Result<PathBuf, String> {
    let mut connection = open(graph)?;
    let transaction = connection.transaction().map_err(|error| error.to_string())?;
    let initialized: Option<String> = transaction
        .query_row(
            "SELECT value FROM graph_meta WHERE key = 'json_version'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let before_by_id = rows_by_id(before);
    let after_by_id = rows_by_id(after);

    if initialized.is_none() {
        transaction
            .execute("DELETE FROM entries", [])
            .map_err(|error| error.to_string())?;
    } else {
        let removed: BTreeSet<&str> = before_by_id
            .keys()
            .map(String::as_str)
            .filter(|id| !after_by_id.contains_key(*id))
            .collect();
        for id in removed {
            transaction
                .execute("DELETE FROM entries WHERE id = ?1", params![id])
                .map_err(|error| error.to_string())?;
        }
    }

    for (ordinal, row) in after.iter().enumerate() {
        let Some(id) = crate::graph_store::entry_id(row) else {
            continue;
        };
        let body = crate::graph_store::to_python_json(row);
        if initialized.is_some() && before_by_id.get(id) == Some(&body) {
            continue;
        }
        transaction
            .execute(
                "INSERT INTO entries(id, ordinal, row) VALUES(?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET ordinal=excluded.ordinal, row=excluded.row",
                params![id, ordinal as i64, body],
            )
            .map_err(|error| error.to_string())?;
    }
    transaction
        .execute(
            "INSERT INTO graph_meta(key, value) VALUES('json_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![json_version],
        )
        .map_err(|error| error.to_string())?;
    transaction.commit().map_err(|error| error.to_string())?;
    Ok(database_path(graph))
}

pub fn read_entries(graph: &Path) -> Result<Vec<Value>, String> {
    let connection = open(graph)?;
    let mut statement = connection
        .prepare("SELECT id, row FROM entries ORDER BY ordinal, id")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .map_err(|error| error.to_string())?;
    let mut entries = Vec::new();
    for row in rows {
        let (id, body) = row.map_err(|error| error.to_string())?;
        let value: Value = serde_json::from_str(&body)
            .map_err(|error| format!("SQLite graph row {id} is invalid JSON: {error}"))?;
        if crate::graph_store::entry_id(&value) != Some(id.as_str()) {
            return Err(format!("SQLite graph row key {id} does not match its content"));
        }
        entries.push(value);
    }
    Ok(entries)
}

pub fn json_version(graph: &Path) -> Result<String, String> {
    let connection = open(graph)?;
    connection
        .query_row(
            "SELECT value FROM graph_meta WHERE key = 'json_version'",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())
}
