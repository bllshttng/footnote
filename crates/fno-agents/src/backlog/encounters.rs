//! The encounters aggregate: one node's encounters[] rows, owned here
//! (ruling 4: each aggregate's module owns its tables - no SQL against
//! `encounters` outside this file).

use super::model::Encounter;
use rusqlite::{params, Connection};

pub const DDL: &str = "CREATE TABLE IF NOT EXISTS encounters (
  node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE, seq INTEGER NOT NULL,
  ts TEXT NOT NULL, evidence TEXT NOT NULL, session_id TEXT, voter_key TEXT, voter_kind TEXT,
  harness TEXT, fno_id TEXT, effort TEXT, model TEXT, extras TEXT NOT NULL DEFAULT '{}',
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

/// Replace one node's encounter rows. The caller owns the transaction; a save
/// touches only this node's rows.
pub fn save(
    connection: &Connection,
    node_id: &str,
    encounters: &[Encounter],
) -> Result<(), String> {
    connection
        .execute(
            "DELETE FROM encounters WHERE node_id = ?1",
            params![node_id],
        )
        .map_err(|error| error.to_string())?;
    for (seq, encounter) in encounters.iter().enumerate() {
        let extras = serde_json::to_string(&encounter.extras).map_err(|error| error.to_string())?;
        connection
            .execute(
                "INSERT INTO encounters (node_id, seq, ts, evidence, session_id, voter_key,
                     voter_kind, harness, fno_id, effort, model, extras)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    node_id,
                    seq as i64,
                    encounter.ts,
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

/// One node's encounters in list order (seq). Schema 3: the extras column
/// round-trips the item keys the typed model keeps as `extras`; an
/// unparsable value reads as empty.
pub fn load(connection: &Connection, node_id: &str) -> Result<Vec<Encounter>, String> {
    let mut statement = connection
        .prepare_cached(
            "SELECT ts, evidence, session_id, voter_key, voter_kind, harness, fno_id, effort,
                    model, extras
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
            ts,
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
            ts,
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
