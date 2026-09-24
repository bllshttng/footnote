//! The findings aggregate: blocking review findings that hold the loop
//! gate until resolved (ruling 4: each aggregate's module owns its tables
//! - no SQL against `findings` outside this file).

use super::model::Finding;
use super::schema_v4::{iso, iso_sql, norm_sql, touch, updated, NOW};
use rusqlite::{params, Connection};

/// Schema 4. created_at is the finding's own wire time, so the table gains
/// only updated_at.
pub fn ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS findings (
  finding_id TEXT PRIMARY KEY,
  node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
  created_at TEXT, body TEXT NOT NULL, block_cmd TEXT, block_excerpt TEXT,
  source_session_id TEXT REFERENCES agent_sessions(id),
  source_harness TEXT REFERENCES harnesses(id), resolved_at TEXT,
  resolved_by_session_id TEXT REFERENCES agent_sessions(id){},
  {},
  {}
);
CREATE INDEX IF NOT EXISTS findings_node_open_idx ON findings(node_id, resolved_at);",
        updated("findings"),
        iso("findings", "created_at"),
        iso("findings", "resolved_at"),
    )
}

pub fn triggers() -> String {
    touch("findings")
}

pub fn ensure_table(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(&ddl())
        .map_err(|error| error.to_string())
}

/// Schema-4 migration copy from `findings_v3`. A stamp that is not UTC
/// ISO-8601 is normalized. A resolved finding whose stamp does not parse
/// stays resolved at the migration time, so it never reopens.
pub(crate) fn copy_from_v3(connection: &Connection) -> Result<(), String> {
    let (created_ok, resolved_ok) = (iso_sql("created_at"), iso_sql("resolved_at"));
    let (created, resolved) = (norm_sql("created_at"), norm_sql("resolved_at"));
    connection
        .execute_batch(&format!(
            "INSERT INTO findings (rowid, finding_id, node_id, created_at, body, block_cmd,
                 block_excerpt, source_session_id, source_harness, resolved_at,
                 resolved_by_session_id, updated_at)
             SELECT rowid, finding_id, node_id,
                    CASE WHEN {created_ok} THEN created_at ELSE {created} END,
                    body, block_cmd, block_excerpt, source_session_id, source_harness,
                    CASE WHEN resolved_at IS NULL THEN NULL WHEN {resolved_ok} THEN resolved_at
                         ELSE COALESCE({resolved}, {NOW}) END,
                    resolved_by_session_id, COALESCE({resolved}, {created}, {NOW})
             FROM findings_v3;"
        ))
        .map_err(|error| format!("schema v4 findings copy: {error}"))
}

/// Replace one node's finding rows. The caller owns the transaction; a save
/// touches only this node's rows.
pub fn save(connection: &Connection, node_id: &str, findings: &[Finding]) -> Result<(), String> {
    connection
        .execute("DELETE FROM findings WHERE node_id = ?1", params![node_id])
        .map_err(|error| error.to_string())?;
    for finding in findings {
        connection
            .execute(
                "INSERT INTO findings (finding_id, node_id, created_at, body, block_cmd,
                     block_excerpt, source_session_id, source_harness, resolved_at,
                     resolved_by_session_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    finding.finding_id,
                    node_id,
                    finding.created_at,
                    finding.body,
                    finding.block_cmd,
                    finding.block_excerpt,
                    finding.source_session_id,
                    finding.source_harness,
                    finding.resolved_at,
                    finding.resolved_by_session_id,
                ],
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

pub fn delete(connection: &Connection, node_id: &str) -> Result<(), String> {
    connection
        .execute("DELETE FROM findings WHERE node_id = ?1", params![node_id])
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// One node's findings, oldest first.
pub fn load(connection: &Connection, node_id: &str) -> Result<Vec<Finding>, String> {
    load_all(connection, Some(node_id), false)
}

/// Findings across the store, optionally scoped to one node and to the
/// still-open ones (an open finding has no resolved_at).
pub fn load_all(
    connection: &Connection,
    node_id: Option<&str>,
    open_only: bool,
) -> Result<Vec<Finding>, String> {
    let mut sql = String::from(
        "SELECT finding_id, node_id, created_at, body, block_cmd, block_excerpt,
                source_session_id, source_harness, resolved_at, resolved_by_session_id
         FROM findings",
    );
    match (node_id, open_only) {
        (Some(_), true) => sql.push_str(" WHERE node_id = ?1 AND resolved_at IS NULL"),
        (Some(_), false) => sql.push_str(" WHERE node_id = ?1"),
        (None, true) => sql.push_str(" WHERE resolved_at IS NULL"),
        (None, false) => {}
    }
    sql.push_str(" ORDER BY created_at, finding_id");
    let mut out = Vec::new();
    if let Some(id) = node_id {
        let mut statement = connection
            .prepare(&sql)
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map(params![id], row_finding)
            .map_err(|error| error.to_string())?;
        for row in rows {
            out.push(row.map_err(|error| error.to_string())?);
        }
    } else {
        let mut statement = connection
            .prepare(&sql)
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], row_finding)
            .map_err(|error| error.to_string())?;
        for row in rows {
            out.push(row.map_err(|error| error.to_string())?);
        }
    }
    Ok(out)
}

fn row_finding(row: &rusqlite::Row) -> rusqlite::Result<Finding> {
    Ok(Finding {
        finding_id: row.get(0)?,
        created_at: row.get(2)?,
        body: row.get(3)?,
        block_cmd: row.get(4)?,
        block_excerpt: row.get(5)?,
        source_session_id: row.get(6)?,
        source_harness: row.get(7)?,
        resolved_at: row.get(8)?,
        resolved_by_session_id: row.get(9)?,
    })
}

/// True when the id is taken by any finding on any node.
pub fn id_exists(connection: &Connection, finding_id: &str) -> Result<bool, String> {
    let taken: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM findings WHERE finding_id = ?1",
            params![finding_id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    Ok(taken > 0)
}

fn finding_ids_in(row: &serde_json::Value) -> Vec<String> {
    row.get("findings")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("finding_id").and_then(serde_json::Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Insert one imported finding under its original id, through the row-JSON
/// mutation path. Ok(true) written, Ok(false) the id is already stored,
/// Err the node row is missing.
pub fn import_finding(
    graph: &std::path::Path,
    node_id: &str,
    finding: Finding,
) -> Result<bool, String> {
    crate::graph_store::mutate_rows(
        graph,
        std::time::Duration::from_secs(5),
        None,
        None,
        |rows| {
            let taken = rows.iter().any(|row| {
                finding_ids_in(row)
                    .iter()
                    .any(|id| *id == finding.finding_id)
            });
            if taken {
                return Ok(false);
            }
            for row in rows.iter_mut() {
                if crate::graph_store::entry_id(row) != Some(node_id) {
                    continue;
                }
                let Ok(mut parsed) = super::model::Node::from_json(row) else {
                    return Ok(false);
                };
                parsed
                    .findings
                    .get_or_insert_with(Vec::new)
                    .push(finding.clone());
                *row = parsed.to_json();
                return Ok(true);
            }
            Err(crate::graph_store::StoreError::Invalid(format!(
                "node {node_id} not found"
            )))
        },
    )
    .map(|landed| landed.is_some())
    .map_err(|error| error.to_string())
}

/// Stamp the resolution on an imported finding by id. Ok(true) stamped,
/// Ok(false) the id is absent or already resolved.
pub fn import_resolve(
    graph: &std::path::Path,
    finding_id: &str,
    resolved_at: &str,
    by_session: Option<&str>,
) -> Result<bool, String> {
    crate::graph_store::mutate_rows(
        graph,
        std::time::Duration::from_secs(5),
        None,
        None,
        |rows| {
            for row in rows.iter_mut() {
                let Ok(mut parsed) = super::model::Node::from_json(row) else {
                    continue;
                };
                let Some(list) = &mut parsed.findings else {
                    continue;
                };
                let Some(finding) = list.iter_mut().find(|f| f.finding_id == finding_id) else {
                    continue;
                };
                if finding.resolved_at.is_some() {
                    return Ok(false);
                }
                finding.resolved_at = Some(resolved_at.to_string());
                finding.resolved_by_session_id = by_session.map(str::to_string);
                *row = parsed.to_json();
                return Ok(true);
            }
            Ok(false)
        },
    )
    .map(|landed| landed.is_some())
    .map_err(|error| error.to_string())
}
