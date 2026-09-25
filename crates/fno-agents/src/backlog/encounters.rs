//! The encounters aggregate: one node's encounters[] rows, owned here
//! (ruling 4: each aggregate's module owns its tables - no SQL against
//! `encounters` outside this file).

use super::model::Encounter;
use super::schema_v4::{iso, norm_sql, touch, updated, NOW};
use rusqlite::{params, Connection};

/// Schema 4. The vote time is the row's creation time, so the schema-3
/// `ts` column is `created_at` now (user ruling 2026-09-23).
pub fn ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS encounters (
  node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE, seq INTEGER NOT NULL,
  created_at TEXT NOT NULL DEFAULT ({NOW}), evidence TEXT NOT NULL,
  session_id TEXT REFERENCES agent_sessions(id), voter_key TEXT, voter_kind TEXT,
  harness TEXT REFERENCES harnesses(id), fno_id TEXT, effort TEXT,
  model TEXT REFERENCES models(id), extras TEXT NOT NULL DEFAULT '{{}}'{},
  {},
  PRIMARY KEY (node_id, seq)
);",
        updated("encounters"),
        iso("encounters", "created_at"),
    )
}

pub fn triggers() -> String {
    touch("encounters")
}

pub fn ensure_table(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(&ddl())
        .map_err(|error| error.to_string())
}

/// Schema-4 migration copy from `encounters_v3`: `ts` becomes created_at.
pub(crate) fn copy_from_v3(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(&format!(
            "INSERT INTO encounters (rowid, node_id, seq, created_at, evidence, session_id,
                 voter_key, voter_kind, harness, fno_id, effort, model, extras, updated_at)
             SELECT rowid, node_id, seq, ts, evidence, session_id, voter_key, voter_kind,
                    harness, fno_id, effort, model, extras, COALESCE({}, {NOW})
             FROM encounters_v3;",
            norm_sql("ts"),
        ))
        .map_err(|error| format!("schema v4 encounters copy: {error}"))
}

/// Schema 3: the unknown item keys the typed model keeps in `extras` gain a
/// column. The schema-4 migration runs it first, so a schema-2 store still
/// carries its rows across.
pub(crate) fn migrate_add_extras(connection: &Connection) -> Result<(), String> {
    let has_extras: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('encounters') WHERE name = 'extras'",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if has_extras > 0 {
        return Ok(());
    }
    if let Err(error) = connection
        .execute_batch("ALTER TABLE encounters ADD COLUMN extras TEXT NOT NULL DEFAULT '{}'")
    {
        // Two first opens can race the ALTER; losing it means the other
        // writer already added the column.
        if !error.to_string().contains("duplicate column name") {
            return Err(error.to_string());
        }
    }
    Ok(())
}

/// Write one node's encounter rows: an upsert per list position that leaves
/// an unchanged row untouched, then a delete of the positions past the
/// list's end. The caller owns the transaction; a save touches only this
/// node's rows.
pub fn save(
    connection: &Connection,
    node_id: &str,
    encounters: &[Encounter],
) -> Result<(), String> {
    for (seq, encounter) in encounters.iter().enumerate() {
        let extras = serde_json::to_string(&encounter.extras).map_err(|error| error.to_string())?;
        connection
            .execute(
                "INSERT INTO encounters (node_id, seq, created_at, evidence, session_id,
                     voter_key, voter_kind, harness, fno_id, effort, model, extras)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT(node_id, seq) DO UPDATE SET created_at = excluded.created_at,
                     evidence = excluded.evidence, session_id = excluded.session_id,
                     voter_key = excluded.voter_key, voter_kind = excluded.voter_kind,
                     harness = excluded.harness, fno_id = excluded.fno_id,
                     effort = excluded.effort, model = excluded.model, extras = excluded.extras
                 WHERE (encounters.created_at, encounters.evidence, encounters.session_id,
                        encounters.voter_key, encounters.voter_kind, encounters.harness,
                        encounters.fno_id, encounters.effort, encounters.model,
                        encounters.extras)
                   IS NOT (excluded.created_at, excluded.evidence, excluded.session_id,
                           excluded.voter_key, excluded.voter_kind, excluded.harness,
                           excluded.fno_id, excluded.effort, excluded.model, excluded.extras)",
                params![
                    node_id,
                    seq as i64,
                    encounter.created_at,
                    encounter.evidence,
                    encounter.session_id,
                    encounter.voter_key,
                    encounter.voter_kind,
                    encounter.harness,
                    encounter.fno_id,
                    encounter.effort,
                    encounter.model,
                    extras,
                ],
            )
            .map_err(|error| error.to_string())?;
    }
    connection
        .execute(
            "DELETE FROM encounters WHERE node_id = ?1 AND seq >= ?2",
            params![node_id, encounters.len() as i64],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

pub fn delete(connection: &Connection, node_id: &str) -> Result<(), String> {
    connection
        .execute(
            "DELETE FROM encounters WHERE node_id = ?1",
            params![node_id],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// One node's encounters in list order (seq). The extras column
/// round-trips the item keys the typed model keeps as `extras`; an
/// unparsable value reads as empty.
pub fn load(connection: &Connection, node_id: &str) -> Result<Vec<Encounter>, String> {
    let mut statement = connection
        .prepare(
            "SELECT created_at, evidence, session_id, voter_key, voter_kind, harness, fno_id,
                    effort, model, extras
             FROM encounters WHERE node_id = ?1 ORDER BY seq",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![node_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
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
            evidence,
            session_id,
            voter_key,
            voter_kind,
            harness,
            fno_id,
            effort,
            model,
            extras_raw,
        ) = row.map_err(|error| error.to_string())?;
        let extras: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&extras_raw).unwrap_or_default();
        out.push(Encounter {
            created_at,
            evidence,
            session_id,
            voter_key,
            voter_kind,
            harness,
            fno_id,
            effort,
            model,
            extras,
        });
    }
    Ok(out)
}
