//! The relations aggregate: one node's blocked_by/related/supersedes lists,
//! stored as relations rows, owned here (ruling 4: each aggregate's module
//! owns its tables - no SQL against `relations` outside this file).
//!
//! Schema 4: both ends of a `relations` row are real nodes. An edge whose
//! far end names no node lives in `relations_unresolved` until that node
//! arrives (the `relations_promote` trigger), and deleting a node parks the
//! edges other nodes list against it (`relations_park`), so a node's lists
//! read back the same either way.

use super::model::Relations;
use super::schema_v4::{stamps, touch};
use rusqlite::{params, Connection, OptionalExtension};

const TABLES: [&str; 2] = ["relations", "relations_unresolved"];

pub fn ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS relations (
  node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,         -- Linear IssueRelation.issue
  related_node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE, -- Linear IssueRelation.relatedIssue
  type TEXT NOT NULL CHECK (type IN ('blocks','related','supersedes')),
  listed_on TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,       -- the node whose JSON list held it
  seq INTEGER NOT NULL{},
  PRIMARY KEY (node_id, related_node_id, type)
);
CREATE INDEX IF NOT EXISTS relations_inverse ON relations(related_node_id, type);
CREATE INDEX IF NOT EXISTS relations_listed_on ON relations(listed_on);
CREATE TABLE IF NOT EXISTS relations_unresolved ( -- an edge whose far end names no node yet
  node_id TEXT NOT NULL, related_node_id TEXT NOT NULL,
  type TEXT NOT NULL CHECK (type IN ('blocks','related','supersedes')),
  listed_on TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
  seq INTEGER NOT NULL{},
  PRIMARY KEY (node_id, related_node_id, type)
);
CREATE INDEX IF NOT EXISTS relations_unresolved_related ON relations_unresolved(related_node_id);
CREATE INDEX IF NOT EXISTS relations_unresolved_listed_on ON relations_unresolved(listed_on);",
        stamps("relations"),
        stamps("relations_unresolved"),
    )
}

/// The park and promote triggers on `nodes`, plus updated_at on both
/// tables. Park runs BEFORE DELETE, while the node row still satisfies the
/// foreign keys; an edge listed on the deleted node itself cascades away.
pub fn triggers() -> String {
    let resolved = "EXISTS (SELECT 1 FROM nodes WHERE id = relations_unresolved.node_id)
           AND EXISTS (SELECT 1 FROM nodes WHERE id = relations_unresolved.related_node_id)";
    format!(
        "{}{}CREATE TRIGGER IF NOT EXISTS relations_park BEFORE DELETE ON nodes BEGIN
  INSERT INTO relations_unresolved (node_id, related_node_id, type, listed_on, seq, created_at)
    SELECT node_id, related_node_id, type, listed_on, seq, created_at FROM relations
    WHERE (node_id = OLD.id OR related_node_id = OLD.id) AND listed_on <> OLD.id
    ON CONFLICT(node_id, related_node_id, type) DO UPDATE SET
      listed_on = excluded.listed_on, seq = excluded.seq;
  DELETE FROM relations WHERE node_id = OLD.id OR related_node_id = OLD.id;
END;
CREATE TRIGGER IF NOT EXISTS relations_promote AFTER INSERT ON nodes BEGIN
  INSERT INTO relations (node_id, related_node_id, type, listed_on, seq, created_at)
    SELECT node_id, related_node_id, type, listed_on, seq, created_at FROM relations_unresolved
    WHERE (node_id = NEW.id OR related_node_id = NEW.id) AND {resolved}
    ON CONFLICT(node_id, related_node_id, type) DO NOTHING;
  DELETE FROM relations_unresolved
    WHERE (node_id = NEW.id OR related_node_id = NEW.id) AND {resolved};
END;
",
        touch("relations"),
        touch("relations_unresolved"),
    )
}

pub fn ensure_table(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(&ddl())
        .map_err(|error| error.to_string())
}

/// Schema-4 migration copy from `relations_v3`. An edge listed on a node
/// that no longer exists had no reader and is dropped; an edge whose far end
/// names no node is parked. Returns (parked, dropped).
pub(crate) fn copy_from_v3(connection: &Connection) -> Result<(i64, i64), String> {
    let count = |sql: &str| -> Result<i64, String> {
        connection
            .query_row(sql, [], |row| row.get(0))
            .map_err(|error| error.to_string())
    };
    let listed = "EXISTS (SELECT 1 FROM nodes_v3 n WHERE n.id = r.listed_on)";
    let both = "EXISTS (SELECT 1 FROM nodes_v3 n WHERE n.id = r.node_id)
                AND EXISTS (SELECT 1 FROM nodes_v3 n WHERE n.id = r.related_node_id)";
    let dropped = count(&format!(
        "SELECT COUNT(*) FROM relations_v3 r WHERE NOT {listed}"
    ))?;
    let parked = count(&format!(
        "SELECT COUNT(*) FROM relations_v3 r WHERE {listed} AND NOT ({both})"
    ))?;
    let stamp = format!(
        "COALESCE({}, {})",
        super::schema_v4::norm_sql(
            "(SELECT n.created_at FROM nodes_v3 n WHERE n.id = r.listed_on)"
        ),
        super::schema_v4::NOW
    );
    connection
        .execute_batch(&format!(
            "INSERT INTO relations (rowid, node_id, related_node_id, type, listed_on, seq,
                 created_at, updated_at)
             SELECT r.rowid, r.node_id, r.related_node_id, r.type, r.listed_on, r.seq,
                    {stamp}, {stamp}
             FROM relations_v3 r WHERE {listed} AND {both};
             INSERT INTO relations_unresolved (node_id, related_node_id, type, listed_on, seq,
                 created_at, updated_at)
             SELECT r.node_id, r.related_node_id, r.type, r.listed_on, r.seq, {stamp}, {stamp}
             FROM relations_v3 r WHERE {listed} AND NOT ({both});"
        ))
        .map_err(|error| format!("schema v4 relations copy: {error}"))?;
    Ok((parked, dropped))
}

/// One node's relation rows as (node_id, related_node_id, type, seq), in
/// list order. blocked_by is stored inverted: a blocker id becomes node_id
/// with type 'blocks', so the edge is queryable from both ends. `seq` counts
/// position within each list. The primary key collapses duplicate (node,
/// related, type) triples, so a repeated id inside one list keeps its first
/// occurrence only.
fn rows_for(node_id: &str, relations: &Relations) -> Vec<(String, String, &'static str, i64)> {
    let mut rows = Vec::new();
    let mut written: std::collections::HashSet<(String, &'static str)> =
        std::collections::HashSet::new();
    for (list, kind) in [
        (&relations.blocked_by, "blocks"),
        (&relations.related, "related"),
        (&relations.supersedes, "supersedes"),
    ] {
        for (seq, item) in list.iter().flatten().enumerate() {
            if !written.insert((item.clone(), kind)) {
                continue;
            }
            let (from, to) = if kind == "blocks" {
                (item.clone(), node_id.to_string())
            } else {
                (node_id.to_string(), item.clone())
            };
            rows.push((from, to, kind, seq as i64));
        }
    }
    rows
}

fn node_exists(connection: &Connection, id: &str) -> Result<bool, String> {
    connection
        .query_row("SELECT 1 FROM nodes WHERE id = ?1", params![id], |_| Ok(()))
        .optional()
        .map(|found| found.is_some())
        .map_err(|error| error.to_string())
}

/// Write one node's relation rows: an upsert per edge that leaves an
/// unchanged row untouched, then a delete of the edges this node no longer
/// lists. An edge whose far end names no node goes to
/// `relations_unresolved`. The caller owns the transaction and saves the
/// node row first; a save touches only rows listed on this node.
pub fn save(connection: &Connection, node_id: &str, relations: &Relations) -> Result<(), String> {
    let rows = rows_for(node_id, relations);
    for (from, to, kind, seq) in &rows {
        let far = if *kind == "blocks" { from } else { to };
        let (table, other) = if node_exists(connection, far)? {
            ("relations", "relations_unresolved")
        } else {
            ("relations_unresolved", "relations")
        };
        connection
            .execute(
                &format!(
                    "INSERT INTO {table} (node_id, related_node_id, type, listed_on, seq)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(node_id, related_node_id, type) DO UPDATE SET
                         listed_on = excluded.listed_on, seq = excluded.seq
                     WHERE ({table}.listed_on, {table}.seq)
                       IS NOT (excluded.listed_on, excluded.seq)"
                ),
                params![from, to, kind, node_id, seq],
            )
            .map_err(|error| error.to_string())?;
        connection
            .execute(
                &format!(
                    "DELETE FROM {other} WHERE node_id = ?1 AND related_node_id = ?2 AND type = ?3"
                ),
                params![from, to, kind],
            )
            .map_err(|error| error.to_string())?;
    }
    let keep: std::collections::HashSet<(&str, &str, &str)> = rows
        .iter()
        .map(|(from, to, kind, _)| (from.as_str(), to.as_str(), *kind))
        .collect();
    for table in TABLES {
        let stored: Vec<(String, String, String)> = {
            let mut statement = connection
                .prepare(&format!(
                    "SELECT node_id, related_node_id, type FROM {table} WHERE listed_on = ?1"
                ))
                .map_err(|error| error.to_string())?;
            let found = statement
                .query_map(params![node_id], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
                .map_err(|error| error.to_string())?;
            found
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?
        };
        for (from, to, kind) in stored {
            if keep.contains(&(from.as_str(), to.as_str(), kind.as_str())) {
                continue;
            }
            connection
                .execute(
                    &format!(
                        "DELETE FROM {table} WHERE node_id = ?1 AND related_node_id = ?2 AND type = ?3"
                    ),
                    params![from, to, kind],
                )
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

pub fn delete(connection: &Connection, node_id: &str) -> Result<(), String> {
    for table in TABLES {
        connection
            .execute(
                &format!("DELETE FROM {table} WHERE listed_on = ?1"),
                params![node_id],
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// One node's relation lists, resolved and parked edges merged. A list with
/// no rows reads as None (the JSON key was absent), not as an empty array.
pub fn load_grouped(connection: &Connection, node_id: &str) -> Result<Relations, String> {
    let mut statement = connection
        .prepare(
            "SELECT node_id, related_node_id, type, seq FROM relations WHERE listed_on = ?1
             UNION ALL
             SELECT node_id, related_node_id, type, seq FROM relations_unresolved
             WHERE listed_on = ?1
             ORDER BY 4",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![node_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    let mut blocked_by: Vec<String> = Vec::new();
    let mut related: Vec<String> = Vec::new();
    let mut supersedes: Vec<String> = Vec::new();
    for row in rows {
        let (row_node_id, related_node_id, relation_type) =
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
