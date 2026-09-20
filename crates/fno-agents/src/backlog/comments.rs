//! The comments aggregate: one node's progress_notes[] rows, owned here
//! (ruling 4: each aggregate's module owns its tables - no SQL against
//! `comments` outside this file).

use super::model::Comment;
use rusqlite::{params, Connection};
use serde_json::{Map, Value};

pub const DDL: &str = "CREATE TABLE IF NOT EXISTS comments (
  node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE, seq INTEGER NOT NULL,
  created_at TEXT, body TEXT, kind TEXT, title TEXT, details TEXT, difficulty TEXT,
  source TEXT, source_session_id TEXT, source_harness TEXT, extras TEXT NOT NULL DEFAULT '{}',
  PRIMARY KEY (node_id, seq)
);";

pub fn ensure_table(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(DDL)
        .map_err(|error| error.to_string())?;
    migrate_add_extras(connection)
}

/// Schema 3: the unknown item keys the typed model keeps in `extras` gain a
/// column, so an item the JSON leg wrote with extra keys no longer diverges
/// from its row after one roundtrip.
fn migrate_add_extras(connection: &Connection) -> Result<(), String> {
    let has_extras: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('comments') WHERE name = 'extras'",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if has_extras > 0 {
        return Ok(());
    }
    if let Err(error) = connection
        .execute_batch("ALTER TABLE comments ADD COLUMN extras TEXT NOT NULL DEFAULT '{}'")
    {
        // Two first opens can race the ALTER; losing it means the other
        // writer already added the column.
        if !error.to_string().contains("duplicate column name") {
            return Err(error.to_string());
        }
    }
    Ok(())
}

/// Replace one node's comment rows. The caller owns the transaction; a save
/// touches only this node's rows.
pub fn save(connection: &Connection, node_id: &str, comments: &[Comment]) -> Result<(), String> {
    connection
        .execute("DELETE FROM comments WHERE node_id = ?1", params![node_id])
        .map_err(|error| error.to_string())?;
    for (seq, comment) in comments.iter().enumerate() {
        let extras = serde_json::to_string(&comment.extras).map_err(|error| error.to_string())?;
        connection
            .execute(
                "INSERT INTO comments (node_id, seq, created_at, body, kind, title, details,
                     difficulty, source, source_session_id, source_harness, extras)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    node_id,
                    seq as i64,
                    comment.created_at,
                    comment.body,
                    comment.kind,
                    comment.title,
                    comment.details,
                    comment.difficulty,
                    comment.source,
                    comment.source_session_id,
                    comment.source_harness,
                    extras,
                ],
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

pub fn delete(connection: &Connection, node_id: &str) -> Result<(), String> {
    connection
        .execute("DELETE FROM comments WHERE node_id = ?1", params![node_id])
        .map_err(|error| error.to_string())?;
    Ok(())
}

type RowParts = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
);

fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RowParts> {
    Ok((
        row.get::<_, Option<String>>(0)?,
        row.get::<_, Option<String>>(1)?,
        row.get::<_, Option<String>>(2)?,
        row.get::<_, Option<String>>(3)?,
        row.get::<_, Option<String>>(4)?,
        row.get::<_, Option<String>>(5)?,
        row.get::<_, Option<String>>(6)?,
        row.get::<_, Option<String>>(7)?,
        row.get::<_, Option<String>>(8)?,
        row.get::<_, String>(9)?,
    ))
}

fn build(parts: RowParts) -> Comment {
    let (
        created_at,
        body,
        kind,
        title,
        details,
        difficulty,
        source,
        source_session_id,
        source_harness,
        extras_raw,
    ) = parts;
    let extras: Map<String, Value> = serde_json::from_str(&extras_raw).unwrap_or_default();
    Comment {
        created_at,
        body,
        kind,
        title,
        details,
        difficulty,
        source,
        source_session_id,
        source_harness,
        extras,
    }
}

/// One node's comments in list order (seq). Schema 3: the extras column
/// round-trips the item keys the typed model keeps as `extras`; an
/// unparsable value reads as empty.
pub fn load(connection: &Connection, node_id: &str) -> Result<Vec<Comment>, String> {
    let mut statement = connection
        .prepare_cached(
            "SELECT created_at, body, kind, title, details, difficulty, source,
                    source_session_id, source_harness, extras
             FROM comments WHERE node_id = ?1 ORDER BY seq",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![node_id], map_row)
        .map_err(|error| error.to_string())?;
    let mut out = Vec::new();
    for row in rows {
        out.push(build(row.map_err(|error| error.to_string())?));
    }
    Ok(out)
}

/// Every node's comments, grouped by node id, in list order within each
/// node. One full scan instead of one query per node.
pub(crate) fn load_all(
    connection: &Connection,
) -> Result<std::collections::HashMap<String, Vec<Comment>>, String> {
    let mut statement = connection
        .prepare_cached(
            "SELECT created_at, body, kind, title, details, difficulty, source,
                    source_session_id, source_harness, extras, node_id
             FROM comments ORDER BY node_id, seq",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| Ok((row.get::<_, String>(10)?, map_row(row)?)))
        .map_err(|error| error.to_string())?;
    let mut out: std::collections::HashMap<String, Vec<Comment>> = std::collections::HashMap::new();
    for row in rows {
        let (node_id, parts) = row.map_err(|error| error.to_string())?;
        out.entry(node_id).or_default().push(build(parts));
    }
    Ok(out)
}
