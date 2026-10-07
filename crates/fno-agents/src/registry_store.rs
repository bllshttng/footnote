//! Registry rows and their schema envelope in the shared graph store.

use crate::state::{Registry, StateError};
use rusqlite::{params, Connection, TransactionBehavior};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const DDL: &str = "CREATE TABLE IF NOT EXISTS registry (
    identity TEXT PRIMARY KEY,
    ordinal INTEGER NOT NULL,
    payload TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS registry_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);";

fn failure(path: &Path, error: impl std::fmt::Display) -> StateError {
    StateError::InvariantViolation(format!("registry store {}: {error}", path.display()))
}

pub(crate) fn database_path(path: &Path) -> Result<PathBuf, StateError> {
    let home = path
        .parent()
        .ok_or_else(|| failure(path, "registry has no parent"))?;
    let root = if home.file_name().is_some_and(|name| name == "agents") {
        home.parent()
            .ok_or_else(|| failure(path, "agents home has no state root"))?
    } else {
        home
    };
    Ok(crate::state_layout::place(root, "graph.json").with_extension("db"))
}

fn open(path: &Path) -> Result<Connection, StateError> {
    let database = database_path(path)?;
    let mut connection = crate::store_conn::open_write(&database).map_err(|e| failure(path, e))?;
    connection
        .execute_batch(DDL)
        .map_err(|e| failure(path, e))?;
    let migrated: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM registry_meta WHERE key='imported')",
            [],
            |r| r.get(0),
        )
        .map_err(|e| failure(path, e))?;
    if !migrated {
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| failure(path, e))?;
        let migrated: bool = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM registry_meta WHERE key='imported')",
                [],
                |r| r.get(0),
            )
            .map_err(|e| failure(path, e))?;
        // No registry flock here: Python's update_registry holds it across
        // this child's read, so taking it again deadlocks. The IMMEDIATE
        // transaction already serializes importers.
        if !migrated {
            let raw = retire_registry(path)?;
            save_document(&transaction, path, raw)?;
            transaction
                .execute(
                    "INSERT INTO registry_meta(key,value) VALUES ('imported','1'),('revision','0')",
                    [],
                )
                .map_err(|e| failure(path, e))?;
            transaction.commit().map_err(|e| failure(path, e))?;
            return Ok(connection);
        }
    }
    Ok(connection)
}

fn save_document(
    connection: &Connection,
    path: &Path,
    mut document: Value,
) -> Result<(), StateError> {
    let object = document
        .as_object_mut()
        .ok_or_else(|| failure(path, "registry is not an object"))?;
    if !object
        .get("schema_version")
        .and_then(Value::as_u64)
        .is_some_and(|v| v >= 1)
    {
        // A wrong-typed version is a plain parse error, not an invariant.
        serde_json::from_value::<Registry>(Value::Object(object.clone()))?;
        return Err(failure(path, "registry has no positive schema_version"));
    }
    let rows = object
        .remove("agents")
        .or_else(|| object.remove("entries"))
        .unwrap_or_else(|| json!([]));
    object.remove("entries");
    let rows = rows
        .as_array()
        .ok_or_else(|| failure(path, "registry rows are not an array"))?;
    connection
        .execute("DELETE FROM registry", [])
        .map_err(|e| failure(path, e))?;
    let mut taken = std::collections::HashSet::new();
    for (ordinal, row) in rows.iter().enumerate() {
        let mut row = row.clone();
        let fields = row
            .as_object_mut()
            .ok_or_else(|| failure(path, "registry row is not an object"))?;
        let session = fields
            .get("harness_session_id")
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                let harness = fields
                    .get("harness")
                    .or_else(|| fields.get("provider"))
                    .and_then(Value::as_str)?;
                let key = match harness {
                    "claude" => "claude_session_uuid",
                    "codex" => "codex_session_id",
                    "gemini" => "gemini_session_id",
                    _ => return None,
                };
                fields
                    .get(key)
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty())
                    .map(str::to_owned)
            });
        let identity = if let Some(session) = session {
            fields.insert("harness_session_id".into(), json!(session));
            // One session id may be live under two harnesses; the harness
            // keeps those rows distinct.
            let harness = fields
                .get("harness")
                .or_else(|| fields.get("provider"))
                .and_then(Value::as_str)
                .unwrap_or("");
            format!("session:{harness}:{session}")
        } else {
            let id = match fields
                .get("fno_id")
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
            {
                Some(id) => id.to_owned(),
                None => {
                    crate::identity::mint_unique_fno_id(&taken).map_err(|e| failure(path, e))?
                }
            };
            fields.insert("fno_id".into(), json!(id));
            taken.insert(id.chars().take(8).collect::<String>().to_ascii_lowercase());
            format!("fno:{id}")
        };
        if !taken.insert(identity.clone()) {
            return Err(failure(
                path,
                format!("duplicate registry identity {identity}"),
            ));
        }
        connection
            .execute(
                "INSERT INTO registry(identity,ordinal,payload) VALUES (?1,?2,?3)",
                params![identity, ordinal as i64, serde_json::to_string(&row)?],
            )
            .map_err(|e| failure(path, e))?;
    }
    connection.execute("INSERT INTO registry_meta(key,value) VALUES ('document',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [serde_json::to_string(&document)?]).map_err(|e| failure(path,e))?;
    Ok(())
}

fn load_document(connection: &Connection, path: &Path) -> Result<Value, StateError> {
    let raw: String = connection
        .query_row(
            "SELECT value FROM registry_meta WHERE key='document'",
            [],
            |r| r.get(0),
        )
        .map_err(|e| failure(path, e))?;
    let mut document: Value = serde_json::from_str(&raw)?;
    let mut statement = connection
        .prepare("SELECT payload FROM registry ORDER BY ordinal")
        .map_err(|e| failure(path, e))?;
    let rows = statement
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| failure(path, e))?;
    let mut values = Vec::new();
    for row in rows {
        values.push(serde_json::from_str::<Value>(
            &row.map_err(|e| failure(path, e))?,
        )?);
    }
    document
        .as_object_mut()
        .ok_or_else(|| failure(path, "registry metadata is not an object"))?
        .insert("agents".into(), Value::Array(values));
    Ok(document)
}

pub fn read(path: &Path) -> Result<Value, StateError> {
    let connection = open(path)?;
    let transaction = connection
        .unchecked_transaction()
        .map_err(|e| failure(path, e))?;
    let document = load_document(&transaction, path)?;
    transaction.commit().map_err(|e| failure(path, e))?;
    Ok(document)
}

pub(crate) struct Write {
    connection: Connection,
    pub(crate) document: Value,
    pub(crate) revision: i64,
    path: PathBuf,
}

pub(crate) fn begin(path: &Path) -> Result<Write, StateError> {
    let connection = open(path)?;
    connection
        .execute_batch("BEGIN IMMEDIATE")
        .map_err(|e| failure(path, e))?;
    let document = load_document(&connection, path)?;
    let revision = revision(&connection, path)?;
    Ok(Write {
        connection,
        document,
        revision,
        path: path.to_path_buf(),
    })
}

impl Write {
    pub(crate) fn commit(self, document: Value) -> Result<(), StateError> {
        crate::state::snapshot_registry(&self.path, &self.document);
        save_document(&self.connection, &self.path, document)?;
        self.connection
            .execute(
                "UPDATE registry_meta SET value=CAST(value AS INTEGER)+1 WHERE key='revision'",
                [],
            )
            .map_err(|e| failure(&self.path, e))?;
        self.connection
            .execute_batch("COMMIT")
            .map_err(|e| failure(&self.path, e))
    }
}

/// Replace the whole registry document behind `path`. Tests seed rows here.
pub fn replace_document(path: &Path, document: Value) {
    begin(path).unwrap().commit(document).unwrap();
}

/// The `agent.watch` version of the registry at `path`, or `None` when it
/// vanished. After the table import the path is a fence directory whose stat
/// never moves, so the table revision rides the `mtime_nanos` slot the agents
/// view already parses; before it, the file's (mtime, len) stamp.
pub fn watch_version(path: &Path) -> Result<Option<serde_json::Value>, crate::state::StateError> {
    if path.is_dir() {
        let (_, revision) = read_versioned(path)?;
        return Ok(Some(serde_json::json!({"mtime_nanos": revision, "len": 0})));
    }
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(crate::state::StateError::Io(e)),
    };
    let mtime_nanos = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    Ok(Some(
        serde_json::json!({"mtime_nanos": mtime_nanos, "len": meta.len()}),
    ))
}

/// Seed `body` as the registry at `path`. Before the table owns the path the
/// bytes land as the legacy file, so the import reads them raw; after, they
/// replace the table document.
pub fn seed_raw(path: impl AsRef<Path>, body: impl AsRef<[u8]>) {
    let path = path.as_ref();
    if path.is_dir() {
        let body = body.as_ref();
        let document = if body.iter().all(u8::is_ascii_whitespace) {
            serde_json::to_value(Registry::default()).unwrap()
        } else {
            serde_json::from_slice(body).unwrap()
        };
        replace_document(path, document);
    } else {
        std::fs::write(path, body).unwrap();
    }
}

/// The registry at `path` as pretty JSON text: the legacy file before the
/// import, the table document after. Tests read rows back here.
pub fn read_raw(path: &Path) -> String {
    if path.is_dir() {
        serde_json::to_string_pretty(&read(path).unwrap()).unwrap()
    } else {
        std::fs::read_to_string(path).unwrap()
    }
}

fn revision(connection: &Connection, path: &Path) -> Result<i64, StateError> {
    connection
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM registry_meta WHERE key='revision'",
            [],
            |r| r.get(0),
        )
        .map_err(|e| failure(path, e))
}

pub(crate) fn read_versioned(path: &Path) -> Result<(Value, i64), StateError> {
    let connection = open(path)?;
    let transaction = connection
        .unchecked_transaction()
        .map_err(|e| failure(path, e))?;
    let document = load_document(&transaction, path)?;
    let revision = revision(&transaction, path)?;
    transaction.commit().map_err(|e| failure(path, e))?;
    Ok((document, revision))
}

fn retire_registry(path: &Path) -> Result<Value, StateError> {
    let parent = path
        .parent()
        .ok_or_else(|| failure(path, "registry has no parent"))?;
    let filename = path
        .file_name()
        .and_then(|v| v.to_str())
        .ok_or_else(|| failure(path, "registry filename is not UTF-8"))?;
    let pending = parent.join(format!(".{filename}.table-fence"));
    let source = parent
        .join("registry-snapshots")
        // Outside the `registry.json.` snapshot prefix, so rotation never
        // counts or prunes the import source.
        .join(format!("legacy.{filename}"));
    if path.is_dir() {
        let marker: Value = serde_json::from_slice(&std::fs::read(path.join("migration.json"))?)?;
        if marker.get("storage") != Some(&json!("graph.db")) {
            return Err(failure(path, "invalid table migration fence"));
        }
        return match marker.get("source").and_then(Value::as_str) {
            Some(source) => Ok(serde_json::from_slice(&std::fs::read(source)?)?),
            None => Ok(serde_json::to_value(Registry::default())?),
        };
    }
    if pending.is_dir() && !path.exists() {
        std::fs::rename(&pending, path)?;
        return retire_registry(path);
    }
    let bytes = match std::fs::read(path) {
        Ok(bytes) if bytes.iter().all(u8::is_ascii_whitespace) => {
            serde_json::to_vec(&Registry::default())?
        }
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            serde_json::to_vec(&Registry::default())?
        }
        Err(e) => return Err(e.into()),
    };
    let raw: Value = serde_json::from_slice(&bytes)?;
    if !raw.is_object() {
        return Err(failure(path, "registry is not an object"));
    }
    std::fs::create_dir_all(source.parent().unwrap())?;
    if source.exists() {
        return Err(failure(
            path,
            "legacy snapshot already exists without a published migration fence",
        ));
    }
    std::fs::create_dir(&pending)?;
    let marker = json!({"storage":"graph.db", "source":source, "remedy":"upgrade fno to read the registry table"});
    let marker_path = pending.join("migration.json");
    use std::io::Write;
    let mut marker_file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&marker_path)?;
    marker_file.write_all(serde_json::to_string(&marker)?.as_bytes())?;
    marker_file.sync_all()?;
    let mut backup = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&source)?;
    backup.write_all(&bytes)?;
    backup.sync_all()?;
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    std::fs::rename(&pending, path)?;
    Ok(raw)
}
