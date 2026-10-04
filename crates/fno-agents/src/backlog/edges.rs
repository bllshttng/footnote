//! The typed edge aggregate: one row per relationship between two entities
//! (ruling d-2327af8e: stable ids, typed rows, append-only). A writer
//! inserts an open row or closes one by stamping valid_to; no SQL against
//! `edges` lives outside this file. `EdgeKind::Backlog` is the existing
//! `relations` rows, read through their own aggregate - this aggregate
//! writes none.

use rusqlite::{params, Connection};

/// The entity table a row's id names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityKind {
    Agent,
    Node,
}

impl EntityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Node => "node",
        }
    }
}

/// One endpoint of an edge: the kind plus the id a key resolves through
/// (an agent's fno_id or session id, a node's id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityRef {
    pub kind: EntityKind,
    pub id: String,
}

impl EntityRef {
    pub fn agent(id: impl Into<String>) -> Self {
        Self {
            kind: EntityKind::Agent,
            id: id.into(),
        }
    }

    pub fn node(id: impl Into<String>) -> Self {
        Self {
            kind: EntityKind::Node,
            id: id.into(),
        }
    }
}

/// The relationship a row carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    Backlog,
    Spawn,
    Message,
}

impl EdgeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Backlog => "backlog",
            Self::Spawn => "spawn",
            Self::Message => "message",
        }
    }
}

pub fn ddl() -> String {
    "CREATE TABLE IF NOT EXISTS edges (
       seq INTEGER PRIMARY KEY,
       src_kind TEXT NOT NULL,
       src_id TEXT NOT NULL,
       dst_kind TEXT NOT NULL,
       dst_id TEXT NOT NULL,
       kind TEXT NOT NULL,
       valid_from TEXT NOT NULL,
       valid_to TEXT
     );
     CREATE INDEX IF NOT EXISTS edges_src_kind ON edges(src_id, kind);
     CREATE INDEX IF NOT EXISTS edges_dst_kind ON edges(dst_id, kind);"
        .to_string()
}

pub fn ensure_table(connection: &Connection) -> Result<(), String> {
    connection.execute_batch(&ddl()).map_err(|e| e.to_string())
}

/// Append one open edge. The only later write stamps valid_to.
pub fn append(
    connection: &Connection,
    src: &EntityRef,
    dst: &EntityRef,
    kind: EdgeKind,
) -> Result<i64, String> {
    connection
        .execute(
            "INSERT INTO edges (src_kind, src_id, dst_kind, dst_id, kind, valid_from)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                src.kind.as_str(),
                src.id,
                dst.kind.as_str(),
                dst.id,
                kind.as_str(),
                crate::daemon::now_rfc3339_like()
            ],
        )
        .map(|_| connection.last_insert_rowid())
        .map_err(|e| e.to_string())
}

/// Close every open row matching the endpoints and kind.
pub fn close(
    connection: &Connection,
    src: &EntityRef,
    dst: &EntityRef,
    kind: EdgeKind,
) -> Result<usize, String> {
    connection
        .execute(
            "UPDATE edges SET valid_to = ?5
             WHERE src_kind = ?1 AND src_id = ?2 AND dst_kind = ?3 AND dst_id = ?4
               AND kind = ?6 AND valid_to IS NULL",
            params![
                src.kind.as_str(),
                src.id,
                dst.kind.as_str(),
                dst.id,
                crate::daemon::now_rfc3339_like(),
                kind.as_str()
            ],
        )
        .map_err(|e| e.to_string())
}

/// The spawn edge, recorded at a birth: parent session -Spawn-> child
/// session. Best effort at the call site: a store failure logs there and
/// never fails the spawn.
pub(crate) fn record_spawn(
    home: &crate::paths::AgentsHome,
    parent_session: &str,
    child_session: &str,
) -> Result<(), String> {
    let root = home
        .root()
        .parent()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let graph = crate::state_layout::place(&root, "graph.json");
    let connection = super::open(&graph)?;
    append(
        &connection,
        &EntityRef::agent(parent_session),
        &EntityRef::agent(child_session),
        EdgeKind::Spawn,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store() -> (TempDir, Connection) {
        let dir = TempDir::new().unwrap();
        let graph = dir.path().join("graph.json");
        std::fs::write(&graph, b"{\"entries\": []}").unwrap();
        let connection = crate::backlog::open(&graph).unwrap();
        (dir, connection)
    }

    fn open_rows(connection: &Connection) -> i64 {
        connection
            .query_row(
                "SELECT COUNT(*) FROM edges WHERE valid_to IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn an_appended_edge_reads_back_open_and_closes() {
        let (_dir, connection) = store();
        let seq = append(
            &connection,
            &EntityRef::agent("p-1"),
            &EntityRef::agent("c-1"),
            EdgeKind::Spawn,
        )
        .unwrap();
        let (kind, src, valid_to): (String, String, Option<String>) = connection
            .query_row(
                "SELECT kind, src_id, valid_to FROM edges WHERE seq = ?1",
                [seq],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(kind, "spawn");
        assert_eq!(src, "p-1");
        assert!(valid_to.is_none());
        let closed = close(
            &connection,
            &EntityRef::agent("p-1"),
            &EntityRef::agent("c-1"),
            EdgeKind::Spawn,
        )
        .unwrap();
        assert_eq!(closed, 1);
        // A re-open appends a new row; the closed one stays closed.
        let again = append(
            &connection,
            &EntityRef::agent("p-1"),
            &EntityRef::agent("c-1"),
            EdgeKind::Spawn,
        )
        .unwrap();
        assert_ne!(seq, again);
        assert_eq!(open_rows(&connection), 1);
    }

    #[test]
    fn close_touches_only_its_own_endpoints() {
        let (_dir, connection) = store();
        append(&connection, &EntityRef::agent("p-1"), &EntityRef::agent("c-1"), EdgeKind::Spawn).unwrap();
        append(&connection, &EntityRef::agent("p-2"), &EntityRef::agent("c-2"), EdgeKind::Spawn).unwrap();
        let closed = close(
            &connection,
            &EntityRef::agent("p-1"),
            &EntityRef::agent("c-1"),
            EdgeKind::Spawn,
        )
        .unwrap();
        assert_eq!(closed, 1);
        assert_eq!(open_rows(&connection), 1);
    }
}
