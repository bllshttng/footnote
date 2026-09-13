//! The encounters aggregate: one node's encounters[] rows, owned here
//! (ruling 4: each aggregate's module owns its tables - no SQL against
//! `encounters` outside this file).

use super::model::Encounter;
use rusqlite::{params, Connection};

pub const DDL: &str = "CREATE TABLE IF NOT EXISTS encounters (
  node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE, seq INTEGER NOT NULL,
  ts TEXT NOT NULL, evidence TEXT NOT NULL, session_id TEXT, voter_key TEXT, voter_kind TEXT,
  harness TEXT, fno_id TEXT, effort TEXT, model TEXT,
  PRIMARY KEY (node_id, seq)
);";

pub fn ensure_table(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(DDL)
        .map_err(|error| error.to_string())
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
        connection
            .execute(
                "INSERT INTO encounters (node_id, seq, ts, evidence, session_id, voter_key,
                     voter_kind, harness, fno_id, effort, model)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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

/// One node's encounters in list order (seq).
pub fn load(connection: &Connection, node_id: &str) -> Result<Vec<Encounter>, String> {
    let mut statement = connection
        .prepare(
            "SELECT ts, evidence, session_id, voter_key, voter_kind, harness, fno_id, effort,
                    model
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
            ))
        })
        .map_err(|error| error.to_string())?;
    let mut out = Vec::new();
    for row in rows {
        let (ts, evidence, session_id, voter_key, voter_kind, harness, fno_id, effort, model) =
            row.map_err(|error| error.to_string())?;
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
            extras: Default::default(),
        });
    }
    Ok(out)
}
