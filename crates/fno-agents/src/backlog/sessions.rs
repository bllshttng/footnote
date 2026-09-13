//! The sessions aggregate: one node's sessions[] rows, owned here (ruling 4:
//! each aggregate's module owns its tables - no SQL against `sessions`
//! outside this file).

use super::model::SessionRecord;
use rusqlite::{params, Connection};
use serde_json::Value;

pub const DDL: &str = "CREATE TABLE IF NOT EXISTS sessions (
  node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE, seq INTEGER NOT NULL,
  phase TEXT NOT NULL, harness TEXT NOT NULL, session_id TEXT NOT NULL,
  started_at TEXT, ended_at TEXT, ended_by TEXT, effort TEXT, at TEXT, claimed_at TEXT,
  observed_model TEXT, merge_grant TEXT,
  PRIMARY KEY (node_id, seq)
);
CREATE INDEX IF NOT EXISTS sessions_by_session ON sessions(session_id, harness);";

pub fn ensure_table(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(DDL)
        .map_err(|error| error.to_string())
}

/// Replace one node's session rows. The caller owns the transaction; a save
/// touches only this node's rows.
pub fn save(
    connection: &Connection,
    node_id: &str,
    sessions: &[SessionRecord],
) -> Result<(), String> {
    connection
        .execute("DELETE FROM sessions WHERE node_id = ?1", params![node_id])
        .map_err(|error| error.to_string())?;
    for (seq, session) in sessions.iter().enumerate() {
        connection
            .execute(
                "INSERT INTO sessions (node_id, seq, phase, harness, session_id, started_at,
                     ended_at, ended_by, effort, at, claimed_at, observed_model, merge_grant)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    node_id,
                    seq as i64,
                    session.phase,
                    session.harness,
                    session.session_id,
                    session.started_at,
                    session.ended_at,
                    session.ended_by,
                    session.effort.as_ref().map(Value::to_string),
                    session.at.as_ref().map(Value::to_string),
                    session.claimed_at.as_ref().map(Value::to_string),
                    session.observed_model.as_ref().map(Value::to_string),
                    session.merge_grant.as_ref().map(Value::to_string),
                ],
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

pub fn delete(connection: &Connection, node_id: &str) -> Result<(), String> {
    connection
        .execute("DELETE FROM sessions WHERE node_id = ?1", params![node_id])
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// One node's sessions in list order (seq).
pub fn load(connection: &Connection, node_id: &str) -> Result<Vec<SessionRecord>, String> {
    let mut statement = connection
        .prepare(
            "SELECT phase, harness, session_id, started_at, ended_at, ended_by, effort, at,
                    claimed_at, observed_model, merge_grant
             FROM sessions WHERE node_id = ?1 ORDER BY seq",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![node_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, Option<String>>(10)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    let mut out = Vec::new();
    for row in rows {
        let (
            phase,
            harness,
            session_id,
            started_at,
            ended_at,
            ended_by,
            effort,
            at,
            claimed_at,
            observed_model,
            merge_grant,
        ) = row.map_err(|error| error.to_string())?;
        out.push(SessionRecord {
            phase,
            harness,
            session_id,
            started_at,
            ended_at,
            ended_by,
            effort: effort.and_then(|v| serde_json::from_str(&v).ok()),
            at: at.and_then(|v| serde_json::from_str(&v).ok()),
            claimed_at: claimed_at.and_then(|v| serde_json::from_str(&v).ok()),
            observed_model: observed_model.and_then(|v| serde_json::from_str(&v).ok()),
            merge_grant: merge_grant.and_then(|v| serde_json::from_str(&v).ok()),
            extras: Default::default(),
        });
    }
    Ok(out)
}
