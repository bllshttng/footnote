//! The one read-only door to the agents registry for code that cannot link
//! the registry store. Before the table import `registry.json` is the file;
//! after it, the path is a fence directory and the rows live in graph.db.

use serde_json::Value;
use std::path::{Path, PathBuf};

/// The graph.db that holds the registry table for the registry at `path`.
pub fn database_path(path: &Path) -> Option<PathBuf> {
    let home = path.parent()?;
    let root = if home.file_name().is_some_and(|name| name == "agents") {
        home.parent()?
    } else {
        home
    };
    Some(crate::state_layout::place(root, "graph.json").with_extension("db"))
}

/// Whether the registry table owns `path`: the fence directory stands there,
/// or graph.db records the import. A plain `registry.json` beside an imported
/// table is a stale pre-import file, never the registry: rm edits the table,
/// so reading that file brings removed rows back. A store that exists but
/// cannot be read counts as owned, so the read fails instead of falling back
/// to the file.
pub fn table_owns(path: &Path) -> bool {
    if path.is_dir() {
        return true;
    }
    if path.file_name().is_none_or(|name| name != "registry.json") {
        return false;
    }
    let Some(database) = database_path(path) else {
        return false;
    };
    if !database.exists() {
        return false;
    }
    let Ok(connection) = crate::store_conn::open_read(&database) else {
        return true;
    };
    let has_meta = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='registry_meta')",
        [],
        |r| r.get::<_, bool>(0),
    );
    match has_meta {
        Ok(false) => false,
        Ok(true) => connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM registry_meta WHERE key='imported')",
                [],
                |r| r.get::<_, bool>(0),
            )
            .unwrap_or(true),
        Err(_) => true,
    }
}

/// The registry document at `path` as JSON text. Before the import a missing
/// path is `NotFound`, exactly as the legacy file read was.
pub fn registry_text(path: &Path) -> std::io::Result<String> {
    if !table_owns(path) {
        return std::fs::read_to_string(path);
    }
    let invalid = |e: String| std::io::Error::new(std::io::ErrorKind::InvalidData, e);
    let database = database_path(path).ok_or_else(|| invalid("registry has no parent".into()))?;
    let connection = crate::store_conn::open_read(&database).map_err(invalid)?;
    let raw: String = connection
        .query_row(
            "SELECT value FROM registry_meta WHERE key='document'",
            [],
            |r| r.get(0),
        )
        .map_err(|e| invalid(e.to_string()))?;
    let mut document: Value = serde_json::from_str(&raw).map_err(|e| invalid(e.to_string()))?;
    let mut statement = connection
        .prepare("SELECT payload FROM registry ORDER BY ordinal")
        .map_err(|e| invalid(e.to_string()))?;
    let rows = statement
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| invalid(e.to_string()))?;
    let mut agents = Vec::new();
    for row in rows {
        let row = row.map_err(|e| invalid(e.to_string()))?;
        agents.push(serde_json::from_str::<Value>(&row).map_err(|e| invalid(e.to_string()))?);
    }
    crate::role_migration::upgrade_registry_rows(&mut agents);
    document
        .as_object_mut()
        .ok_or_else(|| invalid("registry metadata is not an object".into()))?
        .insert("agents".into(), Value::Array(agents));
    Ok(document.to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_imported_registry_reads_its_rows_through_the_fence() {
        let temp = tempfile::tempdir().unwrap();
        let agents = temp.path().join("agents");
        std::fs::create_dir_all(agents.join("registry.json")).unwrap();
        let db = super::database_path(&agents.join("registry.json")).unwrap();
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch(
            "CREATE TABLE registry (identity TEXT, ordinal INTEGER, payload TEXT);
             CREATE TABLE registry_meta (key TEXT, value TEXT);
             INSERT INTO registry_meta VALUES ('document','{\"schema_version\":6}');
             INSERT INTO registry VALUES ('fno:a',0,'{\"fno_id\":\"a\"}');",
        )
        .unwrap();
        drop(c);
        let text = super::registry_text(&agents.join("registry.json")).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(doc["agents"][0]["fno_id"], "a");
        assert_eq!(doc["schema_version"], 6);
    }

    #[test]
    fn a_stale_file_beside_an_imported_table_is_never_read() {
        let temp = tempfile::tempdir().unwrap();
        let agents = temp.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let path = agents.join("registry.json");
        std::fs::write(&path, r#"{"agents":[{"name":"ghost"}]}"#).unwrap();
        assert!(
            !super::table_owns(&path),
            "no store yet: the file is the registry"
        );
        let db = super::database_path(&path).unwrap();
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch(
            "CREATE TABLE registry (identity TEXT, ordinal INTEGER, payload TEXT);
             CREATE TABLE registry_meta (key TEXT, value TEXT);
             INSERT INTO registry_meta VALUES ('document','{}');",
        )
        .unwrap();
        assert!(
            !super::table_owns(&path),
            "a table with no import marker owns nothing"
        );
        c.execute_batch("INSERT INTO registry_meta VALUES ('imported','1');")
            .unwrap();
        drop(c);
        assert!(super::table_owns(&path));
        let text = super::registry_text(&path).unwrap();
        assert!(!text.contains("ghost"), "{text}");
        std::fs::remove_file(&path).unwrap();
        assert!(
            super::registry_text(&path).is_ok(),
            "a missing path still reads the table"
        );
    }
}
