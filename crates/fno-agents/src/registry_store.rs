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
    crate::registry_read::database_path(path)
        .ok_or_else(|| failure(path, "registry has no state root"))
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
            let raw = drop_removed(path, retire_registry(path)?);
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
    // Best effort: every reader already ignores this file through
    // `registry_read::table_owns`; the fence only keeps older binaries off it.
    let _ = refence_stray_file(path);
    Ok(connection)
}

/// A plain registry file beside an imported table is a stale pre-import copy
/// that an older binary or a hand restore wrote. Move it into the snapshots
/// and put the fence back. The fence names no source, so a later re-import
/// starts empty instead of from the stale rows.
fn refence_stray_file(path: &Path) -> Result<(), StateError> {
    if !path.is_file() {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| failure(path, "registry has no parent"))?;
    let filename = path
        .file_name()
        .and_then(|v| v.to_str())
        .ok_or_else(|| failure(path, "registry filename is not UTF-8"))?;
    let pending = parent.join(format!(".{filename}.table-fence"));
    let snapshots = parent.join("registry-snapshots");
    std::fs::create_dir_all(&snapshots)?;
    match std::fs::create_dir(&pending) {
        Ok(()) => {
            let marker =
                json!({"storage":"graph.db", "remedy":"upgrade fno to read the registry table"});
            std::fs::write(pending.join("migration.json"), serde_json::to_vec(&marker)?)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Outside the `registry.json.` snapshot prefix, so rotation never prunes it.
    let aside = snapshots.join(format!("stray.{filename}.{stamp}"));
    match std::fs::rename(path, &aside) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    if path.is_dir() {
        return Ok(());
    }
    std::fs::rename(&pending, path)?;
    Ok(())
}

/// Rows `fno agents rm` removed stay removed through an import: drop every
/// row whose harness session a tombstone beside the registry names. Session
/// ids are unique across harnesses, so the id alone decides.
fn drop_removed(path: &Path, mut raw: Value) -> Value {
    let stones: Vec<Value> = path
        .parent()
        .and_then(|p| std::fs::read(p.join("rm_tombstones.json")).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let removed: std::collections::HashSet<&str> = stones
        .iter()
        .filter_map(|s| s.get("session_id")?.as_str())
        .filter(|s| !s.is_empty())
        .collect();
    // The legacy per-harness keys too: `save_document` folds them into
    // `harness_session_id` only after this filter runs.
    const SESSION_KEYS: [&str; 4] = [
        "harness_session_id",
        "claude_session_uuid",
        "codex_session_id",
        "gemini_session_id",
    ];
    if let Some(rows) = raw.get_mut("agents").and_then(Value::as_array_mut) {
        rows.retain(|row| {
            !SESSION_KEYS
                .iter()
                .filter_map(|key| row.get(*key).and_then(Value::as_str))
                .any(|session| removed.contains(session))
        });
    }
    raw
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn import_drops_removed_rows_and_a_stale_file_is_fenced_off() {
        let temp = tempfile::tempdir().unwrap();
        let agents = temp.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let path = agents.join("registry.json");
        let row = |name: &str, sid: &str| json!({"name": name, "harness": "claude", "claude_session_uuid": sid});
        let legacy = json!({"schema_version": 4, "agents": [row("kept", "s-kept"), row("ghost", "s-ghost")]});
        std::fs::write(&path, legacy.to_string()).unwrap();
        std::fs::write(
            agents.join("rm_tombstones.json"),
            json!([{"harness": "claude", "session_id": "s-ghost", "removed_at": 0}]).to_string(),
        )
        .unwrap();
        let names = |doc: serde_json::Value| -> Vec<String> {
            doc["agents"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["name"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(names(super::read(&path).unwrap()), ["kept"]);
        assert!(path.is_dir(), "the import leaves the fence");

        // A pre-import copy written back over the fence is moved aside on the
        // next open, and no reader serves its rows.
        std::fs::remove_dir_all(&path).unwrap();
        std::fs::write(&path, legacy.to_string()).unwrap();
        assert_eq!(names(super::read(&path).unwrap()), ["kept"]);
        assert!(path.is_dir(), "the stale file is fenced off");
        let aside = std::fs::read_dir(agents.join("registry-snapshots"))
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("stray.registry.json.")
            });
        assert!(aside, "the stale file is kept, not deleted");
    }
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
    crate::role_migration::upgrade_registry_rows(&mut values);
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
    /// Returns whether the write changed the stored document. A write that
    /// stores what was already there rolls back: no revision bump and no wake
    /// for watchers. It still snapshots what it read, so the collapse pin
    /// fires on the first write after a collapse.
    pub(crate) fn commit(self, document: Value) -> Result<bool, StateError> {
        crate::state::snapshot_registry(&self.path, &self.document);
        // The stored form is the real test; the input compare only skips the
        // rewrite when the caller already holds that form.
        if !same_content(&document, &self.document) {
            save_document(&self.connection, &self.path, document)?;
            if !same_content(
                &load_document(&self.connection, &self.path)?,
                &self.document,
            ) {
                self.connection
                    .execute(
                        "UPDATE registry_meta SET value=CAST(value AS INTEGER)+1 WHERE key='revision'",
                        [],
                    )
                    .map_err(|e| failure(&self.path, e))?;
                self.connection
                    .execute_batch("COMMIT")
                    .map_err(|e| failure(&self.path, e))?;
                return Ok(true);
            }
        }
        self.connection
            .execute_batch("ROLLBACK")
            .map_err(|e| failure(&self.path, e))?;
        Ok(false)
    }
}

/// Two registry documents hold the same content when they differ at most in
/// `writer_rev`: the two write doors stamp different values, and a write that
/// changes nothing else must not count as a change.
fn same_content(a: &Value, b: &Value) -> bool {
    let strip = |v: &Value| {
        let mut v = v.clone();
        if let Some(map) = v.as_object_mut() {
            map.remove("writer_rev");
        }
        v
    };
    strip(a) == strip(b)
}

/// Replace the whole registry document behind `path`. Tests seed rows here.
pub fn replace_document(path: &Path, document: Value) {
    begin(path).unwrap().commit(document).unwrap();
}

/// The `agent.watch` version of the registry at `path`, or `None` when it
/// vanished. After the table import no file stat moves, so the table
/// revision rides the `mtime_nanos` slot the agents
/// view already parses; before it, the file's (mtime, len) stamp.
pub fn watch_version(path: &Path) -> Result<Option<serde_json::Value>, crate::state::StateError> {
    if crate::registry_read::table_owns(path) {
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
    if crate::registry_read::table_owns(path) {
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
    if crate::registry_read::table_owns(path) {
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
