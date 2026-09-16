//! Native claim command helpers.
//!
//! Wave 13 keeps the claim decision in Rust and gives the Python compatibility
//! layer one JSON door. Wave 14 moves these helpers from the lockfile adapter
//! to the graph store without changing the command contract.

use crate::claims::{self, AcquireOpts, ClaimRecord, ClaimState};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const DDL: &str = "CREATE TABLE IF NOT EXISTS claims (
  key TEXT PRIMARY KEY,
  holder TEXT NOT NULL,
  schema_version INTEGER NOT NULL,
  acquired_at INTEGER NOT NULL,
  expires_at INTEGER,
  pid INTEGER,
  pid_unavailable INTEGER NOT NULL DEFAULT 0,
  host TEXT NOT NULL,
  machine_id TEXT,
  reason TEXT,
  harness TEXT,
  session_id TEXT,
  pid_provenance TEXT,
  metadata TEXT NOT NULL DEFAULT '{}'
);
CREATE VIEW IF NOT EXISTS node_claims AS
  SELECT key, holder, schema_version, acquired_at, expires_at, pid,
         pid_unavailable, host, machine_id, reason, harness, session_id,
         pid_provenance, metadata
  FROM claims WHERE key LIKE 'node:%';
CREATE TABLE IF NOT EXISTS claim_meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);";

fn root_path(root: Option<&Path>) -> Result<PathBuf, String> {
    root.map(Path::to_path_buf)
        .or_else(claims::global_claims_root)
        .ok_or_else(|| "claims root is unavailable".to_string())
}

fn database_path(root: Option<&Path>) -> Result<PathBuf, String> {
    Ok(root_path(root)?.join("graph.db"))
}

pub fn open(root: Option<&Path>) -> Result<Connection, String> {
    let path = database_path(root)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let mut connection = Connection::open(path).map_err(|error| error.to_string())?;
    connection
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
        .map_err(|error| error.to_string())?;
    connection
        .execute_batch(DDL)
        .map_err(|error| error.to_string())?;
    import_lockfiles(&mut connection, root)?;
    Ok(connection)
}

fn import_lockfiles(connection: &mut Connection, root: Option<&Path>) -> Result<(), String> {
    let imported: Option<String> = connection
        .query_row(
            "SELECT value FROM claim_meta WHERE key = 'lockfiles_imported'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if imported.is_some() {
        return Ok(());
    }
    let directory = claims_dir(root)?;
    let records = if directory.is_dir() {
        claims::list_in(std::slice::from_ref(&directory), None, true)?
    } else {
        Vec::new()
    };
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    for record in records {
        insert_record(&transaction, &record)?;
    }
    transaction
        .execute(
            "INSERT INTO claim_meta (key, value) VALUES ('lockfiles_imported', '1')",
            [],
        )
        .map_err(|error| error.to_string())?;
    transaction.commit().map_err(|error| error.to_string())
}

fn insert_record(connection: &Connection, record: &claims::ClaimRecord) -> Result<(), String> {
    let metadata = serde_json::to_string(&record.metadata).map_err(|error| error.to_string())?;
    connection
        .execute(
            "INSERT OR IGNORE INTO claims
             (key, holder, schema_version, acquired_at, expires_at, pid,
              pid_unavailable, host, machine_id, reason, harness, session_id,
              pid_provenance, metadata)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                record.key,
                record.holder,
                record.schema_version,
                record.acquired_at,
                record.expires_at,
                record.pid,
                record.pid_unavailable,
                record.host,
                record.machine_id,
                record.reason,
                record.harness,
                record.session_id,
                record.pid_provenance,
                metadata,
            ],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

pub fn export_lockfiles(root: Option<&Path>) -> Result<Value, String> {
    let connection = open(root)?;
    let directory = claims_dir(root)?;
    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let mut statement = connection
        .prepare(
            "SELECT key, holder, schema_version, acquired_at, expires_at, pid,
                  pid_unavailable, host, machine_id, reason, harness, session_id,
                  pid_provenance, metadata FROM claims ORDER BY key",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok(claims::ClaimRecord {
                key: row.get(0)?,
                holder: row.get(1)?,
                schema_version: row.get(2)?,
                acquired_at: row.get(3)?,
                expires_at: row.get(4)?,
                pid: row.get(5)?,
                pid_unavailable: row.get(6)?,
                host: row.get(7)?,
                machine_id: row.get(8)?,
                reason: row.get(9)?,
                harness: row.get(10)?,
                session_id: row.get(11)?,
                pid_provenance: row.get(12)?,
                metadata: serde_json::from_str(&row.get::<_, String>(13)?).unwrap_or_default(),
            })
        })
        .map_err(|error| error.to_string())?;
    let mut exported = 0usize;
    for row in rows {
        let record = row.map_err(|error| error.to_string())?;
        let path = claims::claim_path(&record.key, root)?;
        let yaml = serde_yaml_ng::to_string(&record).map_err(|error| error.to_string())?;
        std::fs::write(path, yaml).map_err(|error| error.to_string())?;
        exported += 1;
    }
    Ok(json!({"exported": exported, "root": directory}))
}

fn record_for(connection: &Connection, key: &str) -> Result<Option<ClaimRecord>, String> {
    connection
        .query_row(
            "SELECT key, holder, schema_version, acquired_at, expires_at, pid,
                    pid_unavailable, host, machine_id, reason, harness, session_id,
                    pid_provenance, metadata FROM claims WHERE key = ?1",
            params![key],
            |row| {
                Ok(ClaimRecord {
                    key: row.get(0)?,
                    holder: row.get(1)?,
                    schema_version: row.get(2)?,
                    acquired_at: row.get(3)?,
                    expires_at: row.get(4)?,
                    pid: row.get(5)?,
                    pid_unavailable: row.get(6)?,
                    host: row.get(7)?,
                    machine_id: row.get(8)?,
                    reason: row.get(9)?,
                    harness: row.get(10)?,
                    session_id: row.get(11)?,
                    pid_provenance: row.get(12)?,
                    metadata: serde_json::from_str(&row.get::<_, String>(13)?).unwrap_or_default(),
                })
            },
        )
        .optional()
        .map_err(|error| error.to_string())
}

fn record_from_options(key: &str, holder: &str, options: &AcquireOpts) -> ClaimRecord {
    let (session_id, harness) = claims::resolve_identity();
    let acquired_at = claims::now_ms();
    ClaimRecord {
        schema_version: if options.pid_unavailable {
            claims::PID_UNAVAILABLE_SCHEMA_VERSION
        } else {
            claims::SCHEMA_VERSION
        },
        key: key.to_string(),
        holder: holder.to_string(),
        acquired_at,
        pid: if options.pid_unavailable {
            None
        } else {
            Some(options.pid.unwrap_or_else(std::process::id) as i32)
        },
        host: claims::hostname(),
        pid_unavailable: options.pid_unavailable,
        expires_at: options.ttl_ms.map(|ttl| acquired_at.saturating_add(ttl)),
        reason: options.reason.clone(),
        harness,
        session_id,
        pid_provenance: Some("ambient".to_string()),
        machine_id: Some(claims::machine_id()).filter(|value| !value.is_empty()),
        metadata: options.metadata.clone().unwrap_or_default(),
    }
}

fn insert_db_record(connection: &Connection, record: &ClaimRecord) -> Result<(), String> {
    let metadata = serde_json::to_string(&record.metadata).map_err(|error| error.to_string())?;
    connection
        .execute(
            "INSERT INTO claims
             (key, holder, schema_version, acquired_at, expires_at, pid,
              pid_unavailable, host, machine_id, reason, harness, session_id,
              pid_provenance, metadata)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                record.key,
                record.holder,
                record.schema_version,
                record.acquired_at,
                record.expires_at,
                record.pid,
                record.pid_unavailable,
                record.host,
                record.machine_id,
                record.reason,
                record.harness,
                record.session_id,
                record.pid_provenance,
                metadata,
            ],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn status_json(record: &ClaimRecord) -> Value {
    let mut value = serde_json::to_value(record).unwrap_or_else(|_| json!({}));
    let now = claims::now_ms();
    let probe = |pid| claims::probe_pid(pid);
    let (state, basis) = claims::classify_with_basis(record, Some(now), &probe);
    let (provably_dead, bucket) = claims::classify_for_sweep(record, Some(now), &probe, None, None);
    let expired = record
        .expires_at
        .is_some_and(|expires_at| now >= expires_at);
    if let Value::Object(map) = &mut value {
        map.insert(
            "state".to_string(),
            Value::String(state.as_str().to_string()),
        );
        map.insert("basis".to_string(), Value::String(basis.to_string()));
        map.insert("expired".to_string(), Value::Bool(expired));
        map.insert("provably_dead".to_string(), Value::Bool(provably_dead));
        map.insert("bucket".to_string(), Value::String(bucket.to_string()));
    }
    value
}

pub fn acquire_db(key: &str, holder: &str, options: &AcquireOpts) -> Result<Value, String> {
    if key.is_empty() || holder.is_empty() {
        return Err("key and holder must be non-empty".to_string());
    }
    if let Ok(path) = claims::claim_path(key, options.root.as_deref()) {
        if path.exists() {
            if let Err(claims::ReadError::Corrupted(error)) = claims::read_claim_file(&path) {
                return Err(format!("claim is corrupted: {error}"));
            }
        }
    }
    let mut connection = open(options.root.as_deref())?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let existing = record_for(&transaction, key)?;
    let previous_acquired_at = existing
        .as_ref()
        .filter(|record| record.holder == holder)
        .map(|record| record.acquired_at);
    if let Some(existing) = existing {
        let state = claims::classify(&existing, None);
        if existing.holder != holder && matches!(state, ClaimState::Live | ClaimState::Suspect) {
            return Ok(json!({
                "outcome": "held_by_other",
                "holder": existing.holder,
                "pid": existing.pid,
                "host": existing.host,
            }));
        }
        transaction
            .execute("DELETE FROM claims WHERE key = ?1", params![key])
            .map_err(|error| error.to_string())?;
    }
    let record = record_from_options(key, holder, options);
    insert_db_record(&transaction, &record)?;
    transaction.commit().map_err(|error| error.to_string())?;
    let mut value = status_json(&record);
    if let Value::Object(map) = &mut value {
        map.insert("outcome".to_string(), Value::String("acquired".to_string()));
    }
    if let Some(previous_acquired_at) = previous_acquired_at {
        let mut data = claims::common_event_data(&record);
        data.insert(
            "previous_acquired_at".to_string(),
            Value::Number(previous_acquired_at.into()),
        );
        claims::emit_audit_event(
            options.events_dir.as_deref(),
            "claim_idempotent_reacquired",
            data,
        );
    }
    Ok(value)
}

pub fn release_db(key: &str, holder: &str, root: Option<&Path>) -> Result<Value, String> {
    let mut connection = open(root)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let existing = record_for(&transaction, key)?;
    if existing
        .as_ref()
        .is_some_and(|record| record.holder == holder)
    {
        transaction
            .execute("DELETE FROM claims WHERE key = ?1", params![key])
            .map_err(|error| error.to_string())?;
    }
    transaction.commit().map_err(|error| error.to_string())?;
    Ok(json!({"outcome": "released", "key": key}))
}

pub fn status_db(key: &str, root: Option<&Path>) -> Result<Value, String> {
    if let Ok(path) = claims::claim_path(key, root) {
        if path.exists() {
            if let Err(claims::ReadError::Corrupted(error)) = claims::read_claim_file(&path) {
                return Ok(json!({
                    "key": key,
                    "state": "corrupted",
                    "error": error,
                    "path": path,
                }));
            }
        }
    }
    let connection = open(root)?;
    Ok(record_for(&connection, key)?
        .map(|record| status_json(&record))
        .unwrap_or_else(|| json!({"key": key, "state": "free"})))
}

pub fn list_db(
    prefix: Option<&str>,
    include_stale: bool,
    root: Option<&Path>,
) -> Result<Value, String> {
    let connection = open(root)?;
    let mut statement = connection
        .prepare("SELECT key FROM claims ORDER BY key")
        .map_err(|error| error.to_string())?;
    let keys = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?;
    let mut rows = Vec::new();
    for key in keys {
        let key = key.map_err(|error| error.to_string())?;
        if prefix.is_some_and(|wanted| !key.starts_with(wanted)) {
            continue;
        }
        let Some(record) = record_for(&connection, &key)? else {
            continue;
        };
        let state = claims::classify(&record, None);
        if !include_stale && !matches!(state, ClaimState::Live | ClaimState::Suspect) {
            continue;
        }
        rows.push(status_json(&record));
    }
    Ok(json!({"rows": rows}))
}

pub fn renew_db(
    key: &str,
    holder: &str,
    ttl_ms: i64,
    root: Option<&Path>,
) -> Result<Value, String> {
    if ttl_ms <= 0 {
        return Err("ttl_ms must be positive".to_string());
    }
    let mut connection = open(root)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let Some(mut record) = record_for(&transaction, key)? else {
        return Ok(json!({"outcome": "unchanged", "refreshed": false, "key": key}));
    };
    if record.holder != holder || record.expires_at.is_none() {
        return Ok(json!({"outcome": "unchanged", "refreshed": false, "key": key}));
    }
    record.expires_at = Some(claims::now_ms().saturating_add(ttl_ms));
    transaction
        .execute(
            "UPDATE claims SET expires_at = ?2 WHERE key = ?1",
            params![key, record.expires_at],
        )
        .map_err(|error| error.to_string())?;
    transaction.commit().map_err(|error| error.to_string())?;
    Ok(json!({"outcome": "renewed", "refreshed": true, "claim": status_json(&record)}))
}

pub fn force_release_db(key: &str, reason: &str, root: Option<&Path>) -> Result<Value, String> {
    if reason.trim().is_empty() {
        return Err("reason must be non-empty for force-release".to_string());
    }
    let mut connection = open(root)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let previous_holder = record_for(&transaction, key)?.map(|record| record.holder);
    let archived = previous_holder.is_some();
    transaction
        .execute("DELETE FROM claims WHERE key = ?1", params![key])
        .map_err(|error| error.to_string())?;
    transaction.commit().map_err(|error| error.to_string())?;
    Ok(json!({
        "key": key,
        "path": database_path(root)?,
        "archived": archived,
        "force_released": archived,
        "previous_holder": previous_holder,
    }))
}

pub fn reap_db(root: Option<&Path>, apply: bool) -> Result<Value, String> {
    let mut connection = open(root)?;
    let rows = list_db(None, true, root)?
        .get("rows")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let stale: Vec<String> = rows
        .iter()
        .filter(|row| row.get("state").and_then(Value::as_str) == Some("stale"))
        .filter_map(|row| row.get("key").and_then(Value::as_str).map(str::to_string))
        .collect();
    let count = |state: &str| {
        rows.iter()
            .filter(|row| row.get("state").and_then(Value::as_str) == Some(state))
            .count()
    };
    if apply && !stale.is_empty() {
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        for key in &stale {
            transaction
                .execute("DELETE FROM claims WHERE key = ?1", params![key])
                .map_err(|error| error.to_string())?;
        }
        transaction.commit().map_err(|error| error.to_string())?;
    }
    Ok(json!({
        "apply": apply,
        "scanned": stale.len(),
        "would_reap": stale.len(),
        "reaped": if apply { stale.len() } else { 0 },
        "reap_failed": [],
        "kept_live": count("live"),
        "kept_suspect": count("suspect"),
        "kept_suspect_alive": 0,
        "kept_suspect_unprobed": count("suspect"),
        "kept_offhost": count("offhost"),
        "corrupted": count("corrupted"),
        "vanished": 0,
        "contended": 0,
    }))
}

pub fn release_stopped(
    name: &str,
    session: Option<&str>,
    root: Option<&Path>,
) -> Result<Value, String> {
    let mut connection = open(root)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let mut statement = transaction
        .prepare("SELECT key, holder FROM claims ORDER BY key")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| error.to_string())?;
    let mut released = Vec::new();
    for row in rows {
        let (key, holder) = row.map_err(|error| error.to_string())?;
        let holder_session = session.filter(|value| !value.is_empty());
        let belongs = holder.strip_prefix("spawn-handover:") == Some(name)
            || holder_session.is_some_and(|value| {
                holder == format!("target-session:{value}")
                    || holder == format!("review-session:{value}")
                    || holder == format!("session:{value}")
            });
        if !belongs {
            continue;
        }
        transaction
            .execute("DELETE FROM claims WHERE key = ?1", params![key])
            .map_err(|error| error.to_string())?;
        released.push(json!({
            "key": key,
            "holder": holder,
            "path": database_path(root)?,
        }));
    }
    drop(statement);
    transaction.commit().map_err(|error| error.to_string())?;
    Ok(json!({
        "released": released,
        "kept": [],
        "scanned": released.len(),
    }))
}

fn claims_dir(root: Option<&Path>) -> Result<PathBuf, String> {
    claims::claims_dir_for(root).ok_or_else(|| "claims root is unavailable".to_string())
}

fn archive_path(path: &Path) -> Result<PathBuf, String> {
    let archive = path
        .parent()
        .ok_or_else(|| "claim path has no parent".to_string())?
        .join(".expired");
    std::fs::create_dir_all(&archive).map_err(|error| error.to_string())?;
    let stamp = claims::now_ms();
    let name = path
        .file_name()
        .ok_or_else(|| "claim path has no filename".to_string())?
        .to_string_lossy();
    Ok(archive.join(format!("{name}.{stamp}")))
}

pub fn force_release(key: &str, reason: &str, root: Option<&Path>) -> Result<Value, String> {
    if key.is_empty() {
        return Err("key must be non-empty".to_string());
    }
    if reason.trim().is_empty() {
        return Err("reason must be non-empty for force-release".to_string());
    }
    let path = claims::claim_path(key, root)?;
    if !path.exists() {
        return Ok(json!({
            "key": key,
            "path": path,
            "archived": false,
            "force_released": false,
            "previous_holder": Value::Null,
        }));
    }
    let previous_holder = claims::read_claim_file(&path)
        .ok()
        .map(|record| record.holder);
    let destination = archive_path(&path)?;
    std::fs::rename(&path, &destination).map_err(|error| error.to_string())?;
    Ok(json!({
        "key": key,
        "path": path,
        "archived": true,
        "force_released": true,
        "previous_holder": previous_holder,
    }))
}

pub fn reap(root: Option<&Path>, apply: bool) -> Result<Value, String> {
    let directory = claims_dir(root)?;
    let records = if directory.is_dir() {
        claims::list_in(std::slice::from_ref(&directory), None, true)?
    } else {
        Vec::new()
    };
    let mut reaped = 0usize;
    let mut would_reap = 0usize;
    let mut failures = Vec::new();
    for record in records {
        if claims::classify(&record, None) != ClaimState::Stale {
            continue;
        }
        would_reap += 1;
        if !apply {
            continue;
        }
        let path = claims::claim_path(&record.key, root)?;
        let destination = archive_path(&path)?;
        match std::fs::rename(&path, &destination) {
            Ok(()) if !path.exists() && destination.exists() => reaped += 1,
            Ok(()) => failures.push(format!("archive verification failed: {}", path.display())),
            Err(error) => failures.push(format!("{}: {error}", path.display())),
        }
    }
    Ok(json!({
        "apply": apply,
        "scanned": would_reap,
        "would_reap": would_reap,
        "reaped": reaped,
        "reap_failed": failures,
        "root": directory,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claims::{AcquireOpts, AcquireOutcome};
    use tempfile::TempDir;

    #[test]
    fn claim_store_imports_lockfiles_and_exposes_node_view() {
        let temp = TempDir::new().unwrap();
        let outcome = claims::acquire(
            "node:store-test",
            "holder",
            AcquireOpts {
                root: Some(temp.path().to_path_buf()),
                pid: Some(std::process::id()),
                ..Default::default()
            },
        );
        assert!(matches!(outcome, AcquireOutcome::Acquired(_)));

        let connection = open(Some(temp.path())).unwrap();
        let claims_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM claims", [], |row| row.get(0))
            .unwrap();
        let node_claims_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM node_claims", [], |row| row.get(0))
            .unwrap();
        assert_eq!(claims_count, 1);
        assert_eq!(node_claims_count, 1);
    }

    #[test]
    fn claim_store_export_lockfiles_has_positive_receipt() {
        let temp = TempDir::new().unwrap();
        let outcome = claims::acquire(
            "node:export-test",
            "holder",
            AcquireOpts {
                root: Some(temp.path().to_path_buf()),
                pid: Some(std::process::id()),
                ..Default::default()
            },
        );
        assert!(matches!(outcome, AcquireOutcome::Acquired(_)));

        let receipt = export_lockfiles(Some(temp.path())).unwrap();

        assert_eq!(receipt["exported"], 1);
        assert!(claims::claim_path("node:export-test", Some(temp.path()))
            .unwrap()
            .exists());
    }

    #[test]
    fn claim_store_release_stopped_removes_session_claims() {
        let temp = TempDir::new().unwrap();
        let options = AcquireOpts {
            root: Some(temp.path().to_path_buf()),
            pid: Some(std::process::id()),
            ..Default::default()
        };
        let outcome = acquire_db("node:stopped-test", "target-session:sess", &options).unwrap();
        assert_eq!(outcome["outcome"], "acquired");

        let receipt = release_stopped("worker", Some("sess"), Some(temp.path())).unwrap();

        assert_eq!(receipt["released"].as_array().unwrap().len(), 1);
        assert_eq!(
            status_db("node:stopped-test", Some(temp.path())).unwrap()["state"],
            "free"
        );
    }
}
