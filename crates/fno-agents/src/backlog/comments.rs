//! The comments aggregate: one node's progress_notes[] rows, owned here
//! (ruling 4: each aggregate's module owns its tables - no SQL against
//! `comments` outside this file).

use super::model::Comment;
use rusqlite::{params, Connection};

pub const DDL: &str = "CREATE TABLE IF NOT EXISTS comments (
  node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE, seq INTEGER NOT NULL,
  created_at TEXT, body TEXT, kind TEXT, title TEXT, details TEXT, difficulty TEXT,
  source TEXT, source_session_id TEXT, source_harness TEXT,
  PRIMARY KEY (node_id, seq)
);";

pub fn ensure_table(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(DDL)
        .map_err(|error| error.to_string())
}

/// Replace one node's comment rows. The caller owns the transaction; a save
/// touches only this node's rows.
pub fn save(connection: &Connection, node_id: &str, comments: &[Comment]) -> Result<(), String> {
    connection
        .execute("DELETE FROM comments WHERE node_id = ?1", params![node_id])
        .map_err(|error| error.to_string())?;
    for (seq, comment) in comments.iter().enumerate() {
        connection
            .execute(
                "INSERT INTO comments (node_id, seq, created_at, body, kind, title, details,
                     difficulty, source, source_session_id, source_harness)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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

/// One node's comments in list order (seq).
pub fn load(connection: &Connection, node_id: &str) -> Result<Vec<Comment>, String> {
    let mut statement = connection
        .prepare(
            "SELECT created_at, body, kind, title, details, difficulty, source,
                    source_session_id, source_harness
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
        ) = row.map_err(|error| error.to_string())?;
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
            extras: Default::default(),
        });
    }
    Ok(out)
}
