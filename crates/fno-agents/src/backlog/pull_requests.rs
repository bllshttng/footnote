//! The pull requests aggregate: one node's primary PR plus additional_prs[]
//! rows, owned here (ruling 4: each aggregate's module owns its tables - no
//! SQL against `pull_requests` outside this file).

use super::model::PullRequest;
use rusqlite::{params, Connection};

pub const DDL: &str = "CREATE TABLE IF NOT EXISTS pull_requests (    -- seq 0 = primary (pr_number, pr_url, merge_status)
  node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE, seq INTEGER NOT NULL,
  number INTEGER, url TEXT, merge_status TEXT, note TEXT, extras TEXT NOT NULL DEFAULT '{}',
  PRIMARY KEY (node_id, seq)
);
CREATE INDEX IF NOT EXISTS pull_requests_number ON pull_requests(number);";

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
            "SELECT COUNT(*) FROM pragma_table_info('pull_requests') WHERE name = 'extras'",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if has_extras > 0 {
        return Ok(());
    }
    if let Err(error) = connection
        .execute_batch("ALTER TABLE pull_requests ADD COLUMN extras TEXT NOT NULL DEFAULT '{}'")
    {
        // Two first opens can race the ALTER; losing it means the other
        // writer already added the column.
        if !error.to_string().contains("duplicate column name") {
            return Err(error.to_string());
        }
    }
    Ok(())
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
        let extras = serde_json::to_string(&pr.extras).map_err(|error| error.to_string())?;
        connection
            .execute(
                "INSERT INTO pull_requests (node_id, seq, number, url, merge_status, note, extras)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    node_id,
                    seq as i64,
                    pr.number,
                    pr.url,
                    pr.merge_status,
                    pr.note,
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
            "DELETE FROM pull_requests WHERE node_id = ?1",
            params![node_id],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// One node's pull requests in list order (seq). Schema 3: the extras
/// column round-trips the item keys the typed model keeps as `extras`; an
/// unparsable value reads as empty.
pub fn load(connection: &Connection, node_id: &str) -> Result<Vec<PullRequest>, String> {
    let mut statement = connection
        .prepare(
            "SELECT number, url, merge_status, note, extras
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
                row.get::<_, String>(4)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    let mut out = Vec::new();
    for row in rows {
        let (number, url, merge_status, note, extras_raw) =
            row.map_err(|error| error.to_string())?;
        let extras: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&extras_raw).unwrap_or_default();
        out.push(PullRequest {
            number,
            url,
            merge_status,
            note,
            extras,
        });
    }
    Ok(out)
}
