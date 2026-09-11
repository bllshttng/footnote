//! The relations aggregate: one node's blocked_by/related/supersedes lists,
//! stored as relations rows, owned here (ruling 4: each aggregate's module
//! owns its tables - no SQL against `relations` outside this file).

use super::model::Relations;
use rusqlite::{params, Connection};

pub const DDL: &str = "CREATE TABLE IF NOT EXISTS relations (
  node_id TEXT NOT NULL,                        -- Linear IssueRelation.issue
  related_node_id TEXT NOT NULL,                -- Linear IssueRelation.relatedIssue
  type TEXT NOT NULL CHECK (type IN ('blocks','related','supersedes')),
  listed_on TEXT NOT NULL,                      -- the node whose JSON list held it
  seq INTEGER NOT NULL,                         -- position in that list
  PRIMARY KEY (node_id, related_node_id, type)
);
CREATE INDEX IF NOT EXISTS relations_inverse ON relations(related_node_id, type);";

pub fn ensure_table(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(DDL)
        .map_err(|error| error.to_string())
}

/// Replace one node's relation rows. The caller owns the transaction; a save
/// touches only rows listed on this node. blocked_by is stored inverted: a
/// blocker id becomes node_id with type 'blocks', so the edge is queryable
/// from both ends. `seq` counts position within each list.
pub fn save(connection: &Connection, node_id: &str, relations: &Relations) -> Result<(), String> {
    connection
        .execute(
            "DELETE FROM relations WHERE listed_on = ?1",
            params![node_id],
        )
        .map_err(|error| error.to_string())?;
    if let Some(blockers) = &relations.blocked_by {
        for (seq, blocker_id) in blockers.iter().enumerate() {
            connection
                .execute(
                    "INSERT INTO relations (node_id, related_node_id, type, listed_on, seq)
                     VALUES (?1, ?2, 'blocks', ?3, ?4)",
                    params![blocker_id, node_id, node_id, seq as i64],
                )
                .map_err(|error| error.to_string())?;
        }
    }
    if let Some(related) = &relations.related {
        for (seq, item) in related.iter().enumerate() {
            connection
                .execute(
                    "INSERT INTO relations (node_id, related_node_id, type, listed_on, seq)
                     VALUES (?1, ?2, 'related', ?3, ?4)",
                    params![node_id, item, node_id, seq as i64],
                )
                .map_err(|error| error.to_string())?;
        }
    }
    if let Some(superseded) = &relations.supersedes {
        for (seq, item) in superseded.iter().enumerate() {
            connection
                .execute(
                    "INSERT INTO relations (node_id, related_node_id, type, listed_on, seq)
                     VALUES (?1, ?2, 'supersedes', ?3, ?4)",
                    params![node_id, item, node_id, seq as i64],
                )
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

pub fn delete(connection: &Connection, node_id: &str) -> Result<(), String> {
    connection
        .execute(
            "DELETE FROM relations WHERE listed_on = ?1",
            params![node_id],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// One node's relation lists. A list with no rows reads as None (the JSON
/// key was absent), not as an empty array.
pub fn load_grouped(connection: &Connection, node_id: &str) -> Result<Relations, String> {
    let mut statement = connection
        .prepare(
            "SELECT node_id, related_node_id, type, listed_on, seq
             FROM relations WHERE listed_on = ?1 ORDER BY seq",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![node_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    let mut blocked_by: Vec<String> = Vec::new();
    let mut related: Vec<String> = Vec::new();
    let mut supersedes: Vec<String> = Vec::new();
    for row in rows {
        let (row_node_id, related_node_id, relation_type, _listed_on, _seq) =
            row.map_err(|error| error.to_string())?;
        if relation_type == "blocks" && related_node_id == node_id {
            blocked_by.push(row_node_id);
        } else if relation_type == "related" && row_node_id == node_id {
            related.push(related_node_id);
        } else if relation_type == "supersedes" && row_node_id == node_id {
            supersedes.push(related_node_id);
        }
    }
    Ok(Relations {
        blocked_by: (!blocked_by.is_empty()).then_some(blocked_by),
        related: (!related.is_empty()).then_some(related),
        supersedes: (!supersedes.is_empty()).then_some(supersedes),
    })
}
