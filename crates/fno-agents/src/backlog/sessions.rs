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
  observed_model TEXT, merge_grant TEXT, extras TEXT NOT NULL DEFAULT '{}',
  PRIMARY KEY (node_id, seq)
);
CREATE INDEX IF NOT EXISTS sessions_by_session ON sessions(session_id, harness);";

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
            "SELECT COUNT(*) FROM pragma_table_info('sessions') WHERE name = 'extras'",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if has_extras > 0 {
        return Ok(());
    }
    if let Err(error) = connection
        .execute_batch("ALTER TABLE sessions ADD COLUMN extras TEXT NOT NULL DEFAULT '{}'")
    {
        // Two first opens can race the ALTER; losing it means the other
        // writer already added the column.
        if !error.to_string().contains("duplicate column name") {
            return Err(error.to_string());
        }
    }
    Ok(())
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
        let extras = serde_json::to_string(&session.extras).map_err(|error| error.to_string())?;
        connection
            .execute(
                "INSERT INTO sessions (node_id, seq, phase, harness, session_id, started_at,
                     ended_at, ended_by, effort, at, claimed_at, observed_model, merge_grant,
                     extras)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
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
                    extras,
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

/// One node's sessions in list order (seq). Schema 3: the extras column
/// round-trips the item keys the typed model keeps as `extras`; an
/// unparsable value reads as empty.
pub fn load(connection: &Connection, node_id: &str) -> Result<Vec<SessionRecord>, String> {
    let mut statement = connection
        .prepare_cached(
            "SELECT phase, harness, session_id, started_at, ended_at, ended_by, effort, at,
                    claimed_at, observed_model, merge_grant, extras
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
                row.get::<_, String>(11)?,
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
            extras_raw,
        ) = row.map_err(|error| error.to_string())?;
        let extras: serde_json::Map<String, Value> =
            serde_json::from_str(&extras_raw).unwrap_or_default();
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
            extras,
        });
    }
    Ok(out)
}

/// Fill ONE open window on a RAW entry's sessions rows: for callers whose
/// entry the typed model refused but that still owes its close. The caller
/// names the window it is settling; returns whether any row closed.
pub fn fill_open_window_raw(
    entry: &mut Value,
    session_id: &str,
    phase: Option<&str>,
    harness: Option<&str>,
    ended_at: &str,
    ended_by: &str,
) -> bool {
    let Some(list) = entry.get_mut("sessions").and_then(Value::as_array_mut) else {
        return false;
    };
    let mut closed = false;
    for record in list.iter_mut() {
        let Some(obj) = record.as_object_mut() else {
            continue;
        };
        if obj.get("session_id").and_then(Value::as_str) == Some(session_id)
            && obj.get("ended_at").and_then(Value::as_str).is_none()
            && phase.map_or(true, |want| {
                obj.get("phase").and_then(Value::as_str) == Some(want)
            })
            && harness.map_or(true, |want| {
                obj.get("harness").and_then(Value::as_str) == Some(want)
            })
        {
            obj.insert("ended_at".into(), Value::String(ended_at.to_string()));
            obj.insert("ended_by".into(), Value::String(ended_by.to_string()));
            closed = true;
        }
    }
    closed
}
