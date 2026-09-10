//! Row store shadowing the canonical graph export.

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

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

pub fn shadow_sync(
    graph: &Path,
    _before: &[Value],
    after: &[Value],
    json_version: &str,
) -> Result<PathBuf, String> {
    sync(graph, after, json_version, true)
}

pub fn authoritative_sync(
    graph: &Path,
    _before: &[Value],
    after: &[Value],
) -> Result<String, String> {
    let version = content_version(after);
    sync(graph, after, &version, false)?;
    Ok(version)
}

fn sync(
    graph: &Path,
    after: &[Value],
    version: &str,
    mark_exported: bool,
) -> Result<PathBuf, String> {
    let mut connection = open(graph)?;
    let transaction = connection
        .transaction()
        .map_err(|error| error.to_string())?;
    let initialized: Option<String> = transaction
        .query_row(
            "SELECT value FROM graph_meta WHERE key = 'version'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let after_by_id: BTreeMap<String, String> = after
        .iter()
        .filter_map(|row| {
            crate::graph_store::entry_id(row)
                .map(|id| (id.to_string(), crate::graph_store::to_python_json(row)))
        })
        .collect();
    let stored_by_id: BTreeMap<String, (i64, String)> = if initialized.is_none() {
        BTreeMap::new()
    } else {
        let mut statement = transaction
            .prepare("SELECT id, ordinal, row FROM entries")
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    (row.get::<_, i64>(1)?, row.get::<_, String>(2)?),
                ))
            })
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<BTreeMap<_, _>, _>>()
            .map_err(|error| error.to_string())?
    };

    if initialized.is_none() {
        transaction
            .execute("DELETE FROM entries", [])
            .map_err(|error| error.to_string())?;
    } else {
        let removed: BTreeSet<&str> = stored_by_id
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
        let ordinal = ordinal as i64;
        if stored_by_id
            .get(id)
            .is_some_and(|(stored_ordinal, stored_body)| {
                *stored_ordinal == ordinal && stored_body == &body
            })
        {
            continue;
        }
        transaction
            .execute(
                "INSERT INTO entries(id, ordinal, row) VALUES(?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET ordinal=excluded.ordinal, row=excluded.row",
                params![id, ordinal, body],
            )
            .map_err(|error| error.to_string())?;
    }
    transaction
        .execute(
            "INSERT INTO graph_meta(key, value) VALUES('version', ?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![version],
        )
        .map_err(|error| error.to_string())?;
    let stamped = now_ms().to_string();
    transaction
        .execute(
            "INSERT INTO graph_meta(key, value) VALUES('updated_ms', ?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![stamped],
        )
        .map_err(|error| error.to_string())?;
    if mark_exported {
        transaction
            .execute(
                "INSERT INTO graph_meta(key, value) VALUES('exported_version', ?1)
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![version],
            )
            .map_err(|error| error.to_string())?;
        transaction
            .execute(
                "INSERT INTO graph_meta(key, value) VALUES('last_export_ms', ?1)
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![stamped],
            )
            .map_err(|error| error.to_string())?;
    }
    transaction.commit().map_err(|error| error.to_string())?;
    Ok(database_path(graph))
}

pub fn read_entries(graph: &Path) -> Result<Vec<Value>, String> {
    let connection = open(graph)?;
    if meta(&connection, "version")?.is_none() {
        return Err("SQLite graph has no version".into());
    }
    let mut statement = connection
        .prepare("SELECT id, row FROM entries ORDER BY ordinal, id")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| error.to_string())?;
    let mut entries = Vec::new();
    for row in rows {
        let (id, body) = row.map_err(|error| error.to_string())?;
        let value: Value = serde_json::from_str(&body)
            .map_err(|error| format!("SQLite graph row {id} is invalid JSON: {error}"))?;
        if crate::graph_store::entry_id(&value) != Some(id.as_str()) {
            return Err(format!(
                "SQLite graph row key {id} does not match its content"
            ));
        }
        entries.push(value);
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
