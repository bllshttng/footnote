//! Full-text search over the backlog. The `nodes_fts` FTS5 mirror of the
//! nodes table is owned here and no other file writes it (ruling 4); the
//! Python `fts` seam reaches it through the keeper's `search` read op.

use super::api::{ApiError, Store};
use rusqlite::{params, Connection};

const DDL: &str = "
CREATE VIRTUAL TABLE IF NOT EXISTS nodes_fts USING fts5(
    id UNINDEXED, title, slug, description,
    content='nodes', content_rowid='rowid', tokenize='unicode61'
);
CREATE TRIGGER IF NOT EXISTS nodes_fts_ai AFTER INSERT ON nodes BEGIN
    INSERT INTO nodes_fts(rowid, id, title, slug, description)
    VALUES (new.rowid, new.id, new.title, new.slug, new.description);
END;
CREATE TRIGGER IF NOT EXISTS nodes_fts_ad AFTER DELETE ON nodes BEGIN
    INSERT INTO nodes_fts(nodes_fts, rowid, id, title, slug, description)
    VALUES ('delete', old.rowid, old.id, old.title, old.slug, old.description);
END;
CREATE TRIGGER IF NOT EXISTS nodes_fts_au AFTER UPDATE ON nodes BEGIN
    INSERT INTO nodes_fts(nodes_fts, rowid, id, title, slug, description)
    VALUES ('delete', old.rowid, old.id, old.title, old.slug, old.description);
    INSERT INTO nodes_fts(rowid, id, title, slug, description)
    VALUES (new.rowid, new.id, new.title, new.slug, new.description);
END;
";

pub fn ensure_table(connection: &Connection) -> Result<(), String> {
    let exists: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='nodes_fts')",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if exists {
        return Ok(());
    }
    connection
        .execute_batch(DDL)
        .map_err(|error| error.to_string())?;
    // An external-content table starts empty; one 'rebuild' folds the
    // existing nodes rows in. The triggers cover everything after.
    connection
        .execute("INSERT INTO nodes_fts(nodes_fts) VALUES('rebuild')", [])
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// Node ids matching the query, best first. Tokens are double-quoted so
/// user text never reads as FTS5 syntax; adjacent tokens are an implicit AND.
pub fn search(store: &Store, query: &str, limit: Option<i64>) -> Result<Vec<String>, ApiError> {
    let connection = crate::backlog::open(&store.graph)?;
    query_nodes(&connection, query, limit).map_err(ApiError::from)
}

fn query_nodes(
    connection: &Connection,
    query: &str,
    limit: Option<i64>,
) -> Result<Vec<String>, String> {
    let mut phrases = Vec::new();
    for token in query.split_whitespace() {
        phrases.push(format!("\"{}\"", token.replace('"', "\"\"")));
    }
    if phrases.is_empty() {
        return Ok(Vec::new());
    }
    let mut statement = connection
        .prepare("SELECT id FROM nodes_fts WHERE nodes_fts MATCH ? ORDER BY rank LIMIT ?")
        .map_err(|error| error.to_string())?;
    let found = statement
        .query_map(params![phrases.join(" "), limit.unwrap_or(-1)], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|error| error.to_string())?;
    let mut ids = Vec::new();
    for row in found {
        ids.push(row.map_err(|error| error.to_string())?);
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backlog::api::Node;
    use crate::backlog::{comments, encounters, nodes, pull_requests, relations, sessions};
    use serde_json::json;

    fn db() -> Connection {
        // The schema subset nodes::save/load touches (the nodes row plus
        // the child aggregates their helpers read and clear). nodes_fts is
        // deliberately absent: tests that need it call ensure_table
        // themselves, so the backfill test seeds into an indexless db.
        let connection = Connection::open_in_memory().unwrap();
        // INSERT OR REPLACE (nodes.rs save) deletes the old row without
        // firing delete triggers unless this is on; the fts triggers
        // depend on them. Mirrors the open() funnel pragma.
        connection
            .execute_batch("PRAGMA recursive_triggers=ON;")
            .unwrap();
        for ensure in [
            nodes::ensure_table,
            sessions::ensure_table,
            comments::ensure_table,
            encounters::ensure_table,
            pull_requests::ensure_table,
            relations::ensure_table,
        ] {
            ensure(&connection).unwrap();
        }
        connection
    }

    fn seed(connection: &Connection, id: &str, title: &str, description: &str) {
        // Test rows are composed through the owning module's API; no raw
        // nodes SQL may live in this file (table_ownership scanner).
        let node = Node::from_json(&json!({
            "id": id, "slug": id, "title": title, "type": "feature",
            "status": "ready", "priority": "p2", "domain": "code",
            "description": description,
            "created_at": "2026-09-11T00:00:00+00:00"
        }))
        .unwrap();
        nodes::save(connection, &node).unwrap();
    }

    #[test]
    fn fts_backfill_folds_existing_rows_into_a_fresh_index() {
        let connection = db();
        seed(&connection, "x-a", "Resume handler", "checkpoint resume");
        seed(&connection, "x-b", "Dispatch loop", "spawn workers");
        ensure_table(&connection).unwrap();
        assert_eq!(
            query_nodes(&connection, "resume", None).unwrap(),
            vec!["x-a"]
        );
    }

    #[test]
    fn fts_triggers_keep_the_index_in_step() {
        let connection = db();
        ensure_table(&connection).unwrap();
        seed(&connection, "x-a", "Resume handler", "checkpoint resume");
        assert_eq!(
            query_nodes(&connection, "resume", None).unwrap(),
            vec!["x-a"]
        );
        let mut node = nodes::load(&connection, "x-a").unwrap().unwrap();
        node.title = "Dispatch loop".into();
        node.description = Some("spawn workers".into());
        nodes::save(&connection, &node).unwrap();
        assert!(query_nodes(&connection, "resume", None).unwrap().is_empty());
        assert_eq!(
            query_nodes(&connection, "dispatch", None).unwrap(),
            vec!["x-a"]
        );
        nodes::delete(&connection, "x-a").unwrap();
        assert!(query_nodes(&connection, "dispatch", None)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn fts_query_syntax_is_neutralized() {
        let connection = db();
        ensure_table(&connection).unwrap();
        seed(&connection, "x-a", "Resume handler", "checkpoint resume");
        assert_eq!(
            query_nodes(&connection, "\"unbalanced quote AND (", None).unwrap(),
            Vec::<String>::new()
        );
    }
}
