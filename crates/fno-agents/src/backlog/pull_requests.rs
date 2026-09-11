//! The pull requests aggregate: one node's primary PR plus additional_prs[]
//! rows, owned here (ruling 4: each aggregate's module owns its tables - no
//! SQL against `pull_requests` outside this file).

use super::model::PullRequest;
use rusqlite::{params, Connection};

pub const DDL: &str = "CREATE TABLE IF NOT EXISTS pull_requests (    -- seq 0 = primary (pr_number, pr_url, merge_status)
  node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE, seq INTEGER NOT NULL,
  number INTEGER, url TEXT, merge_status TEXT, note TEXT,
  PRIMARY KEY (node_id, seq)
);
CREATE INDEX IF NOT EXISTS pull_requests_number ON pull_requests(number);";

pub fn ensure_table(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(DDL)
        .map_err(|error| error.to_string())
}

/// Replace one node's PR rows. The caller owns the transaction; a save
/// touches only this node's rows.
pub fn save(
    connection: &Connection,
    node_id: &str,
    pull_requests: &[PullRequest],
) -> Result<(), String> {
    connection
        .execute(
            "DELETE FROM pull_requests WHERE node_id = ?1",
            params![node_id],
        )
        .map_err(|error| error.to_string())?;
    for (seq, pr) in pull_requests.iter().enumerate() {
        connection
            .execute(
                "INSERT INTO pull_requests (node_id, seq, number, url, merge_status, note)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    node_id,
                    seq as i64,
                    pr.number,
                    pr.url,
                    pr.merge_status,
                    pr.note,
                ],
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

pub fn delete(connection: &Connection, node_id: &str) -> Result<(), String> {
    connection
        .execute(
            "DELETE FROM pull_requests WHERE node_id = ?1",
            params![node_id],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// One node's pull requests in list order (seq).
pub fn load(connection: &Connection, node_id: &str) -> Result<Vec<PullRequest>, String> {
    let mut statement = connection
        .prepare(
            "SELECT number, url, merge_status, note
             FROM pull_requests WHERE node_id = ?1 ORDER BY seq",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![node_id], |row| {
            Ok((
                row.get::<_, Option<i64>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    let mut out = Vec::new();
    for row in rows {
        let (number, url, merge_status, note) = row.map_err(|error| error.to_string())?;
        out.push(PullRequest {
            number,
            url,
            merge_status,
            note,
            extras: Default::default(),
        });
    }
    Ok(out)
}
