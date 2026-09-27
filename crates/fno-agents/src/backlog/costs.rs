//! The costs aggregate: one node's cost_sessions[] rows, owned here
//! (ruling 4: each aggregate's module owns its tables - no SQL against
//! `node_costs` outside this file). A list whose items all name a session
//! and carry a UTC ISO-8601 timestamp (or none) lives here; any other list
//! rides nodes.extras as before, so a CHECK never refuses a node save.

use super::model::CostRecord;
use super::schema_v4::{is_utc_iso, iso, iso_sql, norm_sql, stamps, touch, NOW};
use rusqlite::{params, Connection};
use serde_json::{Map, Value};

pub fn ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS node_costs (
  node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE, seq INTEGER NOT NULL,
  session_id TEXT NOT NULL REFERENCES agent_sessions(id), cost_usd REAL NOT NULL,
  timestamp TEXT, extras TEXT NOT NULL DEFAULT '{{}}'{},
  {},
  PRIMARY KEY (node_id, seq)
);",
        stamps("node_costs"),
        iso("node_costs", "timestamp"),
    )
}

pub fn triggers() -> String {
    touch("node_costs")
}

pub fn ensure_table(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(&ddl())
        .map_err(|error| error.to_string())
}

/// Whether node_costs can hold this list. An empty list stays in extras,
/// since rows cannot tell it from an absent key.
pub fn holds(list: &[CostRecord]) -> bool {
    !list.is_empty()
        && list.iter().all(|cost| {
            !cost.session_id.is_empty() && cost.timestamp.as_deref().is_none_or(is_utc_iso)
        })
}

/// Write one node's cost rows: an upsert per list position that leaves an
/// unchanged row untouched, then a delete of the positions past the list's
/// end. A list [`holds`] refuses is written as no rows. The caller owns the
/// transaction.
pub fn save(connection: &Connection, node_id: &str, list: &[CostRecord]) -> Result<(), String> {
    let list = if holds(list) { list } else { &[] };
    for (seq, cost) in list.iter().enumerate() {
        let extras = serde_json::to_string(&cost.extras).map_err(|error| error.to_string())?;
        connection
            .execute(
                "INSERT INTO node_costs (node_id, seq, session_id, cost_usd, timestamp, extras)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(node_id, seq) DO UPDATE SET session_id = excluded.session_id,
                     cost_usd = excluded.cost_usd, timestamp = excluded.timestamp,
                     extras = excluded.extras
                 WHERE (node_costs.session_id, node_costs.cost_usd, node_costs.timestamp,
                        node_costs.extras)
                   IS NOT (excluded.session_id, excluded.cost_usd, excluded.timestamp,
                           excluded.extras)",
                params![
                    node_id,
                    seq as i64,
                    cost.session_id,
                    cost.cost_usd,
                    cost.timestamp,
                    extras
                ],
            )
            .map_err(|error| error.to_string())?;
    }
    connection
        .execute(
            "DELETE FROM node_costs WHERE node_id = ?1 AND seq >= ?2",
            params![node_id, list.len() as i64],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

pub fn delete(connection: &Connection, node_id: &str) -> Result<(), String> {
    connection
        .execute(
            "DELETE FROM node_costs WHERE node_id = ?1",
            params![node_id],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// One node's cost rows in list order. An unparsable extras value reads as
/// empty.
pub fn load(connection: &Connection, node_id: &str) -> Result<Vec<CostRecord>, String> {
    let mut statement = connection
        .prepare(
            "SELECT session_id, cost_usd, timestamp, extras FROM node_costs
             WHERE node_id = ?1 ORDER BY seq",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![node_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, f64>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    let mut out = Vec::new();
    for row in rows {
        let (session_id, cost_usd, timestamp, extras_raw) =
            row.map_err(|error| error.to_string())?;
        let extras: Map<String, Value> = serde_json::from_str(&extras_raw).unwrap_or_default();
        out.push(CostRecord {
            session_id,
            cost_usd,
            timestamp,
            extras,
        });
    }
    Ok(out)
}

/// Schema-4 migration step: copy each node's `extras.cost_sessions` list
/// into node_costs when [`holds`] would accept it, the same rule in SQL.
/// Runs after the nodes copy, before any trigger exists; the caller drops
/// the key from those nodes' extras (nodes.rs owns that table). Returns
/// (the promoted node ids, how many nodes keep their list in extras).
pub(crate) fn promote_from_extras(connection: &Connection) -> Result<(Vec<String>, i64), String> {
    let timestamp = "json_extract(j.value, '$.timestamp')";
    // json_each yields a string item unquoted, which json_type cannot read,
    // so the object checks run only on object items.
    let refused = format!(
        "CASE WHEN j.type <> 'object' THEN 1
              WHEN COALESCE(json_type(j.value, '$.session_id'), '') <> 'text' THEN 1
              WHEN json_extract(j.value, '$.session_id') = '' THEN 1
              WHEN COALESCE(json_type(j.value, '$.cost_usd'), '') NOT IN ('integer', 'real')
                   THEN 1
              WHEN COALESCE(json_type(j.value, '$.timestamp'), 'null') = 'null' THEN 0
              WHEN json_type(j.value, '$.timestamp') <> 'text' THEN 1
              WHEN COALESCE({ok}, 0) THEN 0
              ELSE 1 END",
        ok = iso_sql(timestamp)
    );
    let stamp = format!(
        "COALESCE({}, {}, {NOW})",
        norm_sql(timestamp),
        norm_sql("n.created_at")
    );
    connection
        .execute_batch(&format!(
            "CREATE TEMP TABLE v4_cost_nodes AS
             SELECT n.id AS id,
                    NOT EXISTS (SELECT 1 FROM json_each(n.extras, '$.cost_sessions') j
                                WHERE {refused}) AS ok
             FROM nodes n
             WHERE json_type(n.extras, '$.cost_sessions') = 'array'
               AND json_array_length(n.extras, '$.cost_sessions') > 0;
             INSERT INTO node_costs (node_id, seq, session_id, cost_usd, timestamp, extras,
                 created_at, updated_at)
             SELECT n.id, j.key, json_extract(j.value, '$.session_id'),
                    json_extract(j.value, '$.cost_usd'), {timestamp},
                    json_remove(j.value, '$.session_id', '$.cost_usd', '$.timestamp'),
                    {stamp}, {stamp}
             FROM nodes n JOIN v4_cost_nodes c ON c.id = n.id AND c.ok,
                  json_each(n.extras, '$.cost_sessions') j;"
        ))
        .map_err(|error| format!("schema v4 cost promotion: {error}"))?;
    let promoted: Vec<String> = {
        let mut statement = connection
            .prepare("SELECT id FROM v4_cost_nodes WHERE ok ORDER BY id")
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| row.get(0))
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<_, _>>()
            .map_err(|error| error.to_string())?
    };
    let kept: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM v4_cost_nodes WHERE NOT ok",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    connection
        .execute_batch("DROP TABLE v4_cost_nodes;")
        .map_err(|error| error.to_string())?;
    Ok((promoted, kept))
}
