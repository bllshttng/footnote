//! The comments aggregate: one node's progress_notes[] rows, owned here
//! (ruling 4: each aggregate's module owns its tables - no SQL against
//! `comments` outside this file).

use super::model::Comment;
use super::schema_v4::{iso, iso_sql, norm_sql, touch, updated, NOW};
use rusqlite::{params, Connection};
use serde_json::{Map, Value};

/// Schema 4. created_at is the note's own wire time (`ts`), so the table
/// gains only updated_at.
pub fn ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS comments (
  node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE, seq INTEGER NOT NULL,
  created_at TEXT, body TEXT, kind TEXT, title TEXT, details TEXT, difficulty TEXT,
  source TEXT, source_session_id TEXT REFERENCES agent_sessions(id),
  source_harness TEXT REFERENCES harnesses(id), extras TEXT NOT NULL DEFAULT '{{}}'{},
  {},
  PRIMARY KEY (node_id, seq)
);",
        updated("comments"),
        iso("comments", "created_at"),
    )
}

pub fn triggers() -> String {
    touch("comments")
}

pub fn ensure_table(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(&ddl())
        .map_err(|error| error.to_string())
}

/// Schema-4 migration copy from `comments_v3`. A note `ts` that is not UTC
/// ISO-8601 moves to extras, as the model reads it.
pub(crate) fn copy_from_v3(connection: &Connection) -> Result<(), String> {
    let ok = iso_sql("c.created_at");
    connection
        .execute_batch(&format!(
            "INSERT INTO comments (rowid, node_id, seq, created_at, body, kind, title, details,
                 difficulty, source, source_session_id, source_harness, extras, updated_at)
             SELECT c.rowid, c.node_id, c.seq, CASE WHEN {ok} THEN c.created_at END,
                    c.body, c.kind, c.title, c.details,
                    c.difficulty, c.source, c.source_session_id, c.source_harness,
                    CASE WHEN c.created_at IS NOT NULL AND NOT COALESCE({ok}, 0)
                         THEN json_set(c.extras, '$.ts', c.created_at) ELSE c.extras END,
                    COALESCE({}, {}, {NOW})
             FROM comments_v3 c LEFT JOIN nodes_v3 n ON n.id = c.node_id;",
            norm_sql("c.created_at"),
            norm_sql("n.created_at"),
        ))
        .map_err(|error| format!("schema v4 comments copy: {error}"))
}

/// Schema 3: the unknown item keys the typed model keeps in `extras` gain a
/// column. The schema-4 migration runs it first, so a schema-2 store still
/// carries its rows across.
pub(crate) fn migrate_add_extras(connection: &Connection) -> Result<(), String> {
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

/// Write one node's comment rows: an upsert per list position that leaves
/// an unchanged row untouched, then a delete of the positions past the
/// list's end. The caller owns the transaction; a save touches only this
/// node's rows.
pub fn save(connection: &Connection, node_id: &str, comments: &[Comment]) -> Result<(), String> {
    for (seq, comment) in comments.iter().enumerate() {
        let extras = serde_json::to_string(&comment.extras).map_err(|error| error.to_string())?;
        connection
            .execute(
                "INSERT INTO comments (node_id, seq, created_at, body, kind, title, details,
                     difficulty, source, source_session_id, source_harness, extras)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT(node_id, seq) DO UPDATE SET created_at = excluded.created_at,
                     body = excluded.body, kind = excluded.kind, title = excluded.title,
                     details = excluded.details, difficulty = excluded.difficulty,
                     source = excluded.source, source_session_id = excluded.source_session_id,
                     source_harness = excluded.source_harness, extras = excluded.extras
                 WHERE (comments.created_at, comments.body, comments.kind, comments.title,
                        comments.details, comments.difficulty, comments.source,
                        comments.source_session_id, comments.source_harness, comments.extras)
                   IS NOT (excluded.created_at, excluded.body, excluded.kind, excluded.title,
                           excluded.details, excluded.difficulty, excluded.source,
                           excluded.source_session_id, excluded.source_harness,
                           excluded.extras)",
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
    connection
        .execute(
            "DELETE FROM comments WHERE node_id = ?1 AND seq >= ?2",
            params![node_id, comments.len() as i64],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

pub fn delete(connection: &Connection, node_id: &str) -> Result<(), String> {
    connection
        .execute("DELETE FROM comments WHERE node_id = ?1", params![node_id])
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// One node's comments in list order (seq). Schema 3: the extras column
/// round-trips the item keys the typed model keeps as `extras`; an
/// unparsable value reads as empty.
pub fn load(connection: &Connection, node_id: &str) -> Result<Vec<Comment>, String> {
    let mut statement = connection
        .prepare(
            "SELECT created_at, body, kind, title, details, difficulty, source,
                    source_session_id, source_harness, extras
             FROM comments WHERE node_id = ?1 ORDER BY seq",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![node_id], |row| {
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
        })
        .map_err(|error| error.to_string())?;
    let mut out = Vec::new();
    for row in rows {
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
        ) = row.map_err(|error| error.to_string())?;
        let extras: Map<String, Value> = serde_json::from_str(&extras_raw).unwrap_or_default();
        out.push(Comment {
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
        });
    }
    Ok(out)
}
