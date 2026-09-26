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

pub(crate) fn database_path(root: Option<&Path>) -> Result<PathBuf, String> {
    Ok(root_path(root)?.join("graph.db"))
}

pub fn open(root: Option<&Path>) -> Result<Connection, String> {
    open_paths(database_path(root)?, claims_dir(root)?)
}

fn open_for_key(key: &str, root: Option<&Path>) -> Result<Connection, String> {
    if root.is_some() || claims::claims_root_for(key).is_some() {
        return open(root);
    }
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let space = crate::paths::space_dir(&cwd);
    open_paths(space.join("graph.db"), space.join("claims"))
}

fn open_paths(path: PathBuf, directory: PathBuf) -> Result<Connection, String> {
    crate::live_store_fence::refuse_worktree_build_on_operator_store(&path)?;
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
    import_lockfiles(&mut connection, &directory)?;
    Ok(connection)
}

fn import_lockfiles(connection: &mut Connection, directory: &Path) -> Result<(), String> {
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
    let records = if directory.is_dir() {
        let directory = directory.to_path_buf();
        // The in-window listing: a pid-less lease inside its window classifies
        // Free and must still fold, or the db loses the holder of record.
        crate::claims::list_in_window(&[directory], None)?
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
    let mut connection = open_for_key(key, options.root.as_deref())?;
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
    } else {
        let mut data = claims::common_event_data(&record);
        if let Some(reason) = &record.reason {
            data.insert("reason".to_string(), Value::String(reason.clone()));
        }
        claims::emit_audit_event(options.events_dir.as_deref(), "claim_acquired", data);
    }
    Ok(value)
}

pub fn release_db(
    key: &str,
    holder: &str,
    root: Option<&Path>,
    events_dir: Option<&Path>,
) -> Result<Value, String> {
    let mut connection = open_for_key(key, root)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let existing = record_for(&transaction, key)?;
    let released = existing
        .as_ref()
        .is_some_and(|record| record.holder == holder);
    if released {
        transaction
            .execute("DELETE FROM claims WHERE key = ?1", params![key])
            .map_err(|error| error.to_string())?;
    }
    transaction.commit().map_err(|error| error.to_string())?;
    if released {
        if let Some(record) = existing {
            let mut data = claims::common_event_data(&record);
            data.insert(
                "duration_held_ms".to_string(),
                Value::Number((claims::now_ms() - record.acquired_at).max(0).into()),
            );
            claims::emit_audit_event(events_dir, "claim_released", data);
        }
    }
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
    let connection = open_for_key(key, root)?;
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
    events_dir: Option<&Path>,
) -> Result<Value, String> {
    if ttl_ms <= 0 {
        return Err("ttl_ms must be positive".to_string());
    }
    let mut connection = open_for_key(key, root)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let Some(mut record) = record_for(&transaction, key)? else {
        return Ok(json!({"outcome": "unchanged", "refreshed": false, "key": key}));
    };
    if record.holder != holder || record.expires_at.is_none() {
        return Ok(json!({"outcome": "unchanged", "refreshed": false, "key": key}));
    }
    let previous_expires_at = record.expires_at;
    record.expires_at = Some(claims::now_ms().saturating_add(ttl_ms));
    transaction
        .execute(
            "UPDATE claims SET expires_at = ?2 WHERE key = ?1",
            params![key, record.expires_at],
        )
        .map_err(|error| error.to_string())?;
    transaction.commit().map_err(|error| error.to_string())?;
    let mut data = claims::common_event_data(&record);
    data.insert(
        "previous_expires_at".to_string(),
        previous_expires_at.map(Value::from).unwrap_or(Value::Null),
    );
    claims::emit_audit_event(events_dir, "claim_refreshed", data);
    Ok(json!({"outcome": "renewed", "refreshed": true, "claim": status_json(&record)}))
}

pub fn force_release_db(
    key: &str,
    reason: &str,
    root: Option<&Path>,
    events_dir: Option<&Path>,
) -> Result<Value, String> {
    if reason.trim().is_empty() {
        return Err("reason must be non-empty for force-release".to_string());
    }
    let mut connection = open_for_key(key, root)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let previous = record_for(&transaction, key)?;
    let previous_holder = previous.as_ref().map(|record| record.holder.clone());
    let archived = previous_holder.is_some();
    transaction
        .execute("DELETE FROM claims WHERE key = ?1", params![key])
        .map_err(|error| error.to_string())?;
    transaction.commit().map_err(|error| error.to_string())?;
    if let Some(record) = previous {
        let mut data = claims::common_event_data(&record);
        data.insert(
            "override_reason".to_string(),
            Value::String(reason.to_string()),
        );
        claims::emit_audit_event(events_dir, "claim_force_overridden", data);
    }
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
    let mut reaped = 0usize;
    if apply && !stale.is_empty() {
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        for key in &stale {
            let still_stale = record_for(&transaction, key)?
                .is_some_and(|record| claims::classify(&record, None) == ClaimState::Stale);
            if !still_stale {
                continue;
            }
            transaction
                .execute("DELETE FROM claims WHERE key = ?1", params![key])
                .map_err(|error| error.to_string())?;
            reaped += 1;
        }
        transaction.commit().map_err(|error| error.to_string())?;
    }
    Ok(json!({
        "apply": apply,
        "scanned": stale.len(),
        "would_reap": stale.len(),
        "reaped": reaped,
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

/// Read one repo-space claims directory using the same root resolution as a
/// rootless claim operation. `claims::list` intentionally reads the global
/// directory plus an explicitly supplied repository root; repo-space claims
/// live directly under `<space>/claims` and need this resolver.
pub fn list_repo_space(prefix: &str, include_stale: bool) -> Result<Vec<ClaimRecord>, String> {
    let key = format!("{prefix}probe");
    let directory = crate::claims_root::claims_dir(&key, None)?;
    claims::list_in(
        std::slice::from_ref(&directory),
        Some(prefix),
        include_stale,
    )
}

pub fn force_release(key: &str, reason: &str, root: Option<&Path>) -> Result<Value, String> {
    if key.is_empty() {
        return Err("key must be non-empty".to_string());
    }
    if reason.trim().is_empty() {
        return Err("reason must be non-empty for force-release".to_string());
    }
    let path = claims::claim_path(key, root)?;
    claims::with_recovery_lock(&path, || {
        if !path.exists() {
            return Ok(json!({
                "key": key,
                "path": path.clone(),
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
            "path": path.clone(),
            "archived": true,
            "force_released": true,
            "previous_holder": previous_holder,
        }))
    })
}

#[cfg(test)]
fn reap_one(path: &Path, expected: &ClaimRecord) -> Result<bool, String> {
    reap_one_with_session_witness(path, expected, None)
}

fn reap_one_with_session_witness(
    path: &Path,
    expected: &ClaimRecord,
    session_witness: Option<claims::SessionWitness<'_>>,
) -> Result<bool, String> {
    claims::with_recovery_lock(path, || {
        let current = match claims::read_claim_file(path) {
            Ok(record) => record,
            Err(claims::ReadError::GoneAway) => return Ok(false),
            Err(claims::ReadError::Corrupted(error)) => {
                return Err(format!("{}: corrupted claim: {error}", path.display()))
            }
        };
        if &current != expected {
            return Ok(false);
        }
        if claims::classify_with_session_witness(&current, session_witness) != ClaimState::Stale {
            return Ok(false);
        }
        let destination = archive_path(path)?;
        std::fs::rename(path, &destination).map_err(|error| error.to_string())?;
        let archived = claims::read_claim_file(&destination).map_err(|error| {
            format!(
                "archive verification failed: {}: {error:?}",
                destination.display()
            )
        })?;
        if archived.key != current.key
            || archived.holder != current.holder
            || archived.acquired_at != current.acquired_at
        {
            return Err(format!(
                "archive verification failed: {} changed during reap",
                path.display()
            ));
        }
        Ok(true)
    })
}

pub fn reap(root: Option<&Path>, apply: bool) -> Result<Value, String> {
    reap_with_session_witness(root, apply, None, None)
}

pub(crate) fn reap_with_session_witness(
    root: Option<&Path>,
    apply: bool,
    session_witness: Option<claims::SessionWitness<'_>>,
    recheck_witness: Option<claims::SessionWitness<'_>>,
) -> Result<Value, String> {
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
        if claims::classify_with_session_witness(&record, session_witness) != ClaimState::Stale {
            continue;
        }
        would_reap += 1;
        if !apply {
            continue;
        }
        let path = claims::claim_path(&record.key, root)?;
        match reap_one_with_session_witness(&path, &record, recheck_witness) {
            Ok(true) => reaped += 1,
            Ok(false) => {}
            Err(error) => failures.push(error),
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

#[cfg(test)]
mod lockfile_tests {

    use super::*;
    use std::thread;
    use std::time::Duration;
    use tempfile::TempDir;

    struct ClaimsRootRestore(Option<std::ffi::OsString>);

    impl Drop for ClaimsRootRestore {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => std::env::set_var("FNO_CLAIMS_ROOT", value),
                None => std::env::remove_var("FNO_CLAIMS_ROOT"),
            }
        }
    }

    fn with_claims_root<T>(root: &Path, f: impl FnOnce() -> T) -> T {
        let _env_lock = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let restore = ClaimsRootRestore(std::env::var_os("FNO_CLAIMS_ROOT"));
        std::env::set_var("FNO_CLAIMS_ROOT", root);
        let result = f();
        drop(restore);
        result
    }

    fn reaped_pid() -> u32 {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    fn live_replacement(old: &ClaimRecord, holder: &str) -> ClaimRecord {
        let mut fresh = old.clone();
        fresh.holder = holder.to_string();
        fresh.acquired_at = claims::now_ms();
        fresh.pid = Some(std::process::id() as i32);
        fresh.session_id = None;
        fresh
    }

    #[test]
    fn repo_space_listing_reads_the_resolved_lockfile_directory() {
        let temp = TempDir::new().unwrap();
        with_claims_root(temp.path(), || {
            let key = "merge-slot:main";
            assert!(matches!(
                claims::acquire(
                    key,
                    "pr:17",
                    claims::AcquireOpts {
                        pid_unavailable: true,
                        ttl_ms: Some(60_000),
                        ..Default::default()
                    }
                ),
                claims::AcquireOutcome::Acquired(_)
            ));

            let records = list_repo_space("merge-slot:", false).unwrap();
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].key, key);
            assert_eq!(records[0].holder, "pr:17");
        });
    }

    #[test]
    fn reap_one_refuses_a_fresh_replacement_after_a_stale_scan() {
        let temp = TempDir::new().unwrap();
        with_claims_root(temp.path(), || {
            let key = "node:reap-race-test";
            let mut old = match claims::acquire(
                key,
                "target-session:old",
                claims::AcquireOpts {
                    pid: Some(reaped_pid()),
                    ..Default::default()
                },
            ) {
                claims::AcquireOutcome::Acquired(record) => record,
                other => panic!("claim fixture failed: {other:?}"),
            };
            let path = claims::claim_path(key, Some(temp.path())).unwrap();
            old.session_id = None;
            std::fs::write(&path, claims::serialize_claim(&old).unwrap()).unwrap();
            let lock = claims::recovery_lock_path(&path);
            let token = claims::acquire_dir_mutex(&lock, Duration::from_secs(2), true).unwrap();
            let fresh = live_replacement(&old, "target-session:fresh");
            std::fs::write(&path, claims::serialize_claim(&fresh).unwrap()).unwrap();
            claims::release_dir_mutex(&lock, &token);

            assert!(!reap_one(&path, &old).unwrap());
            assert_eq!(
                claims::status(key, Some(temp.path())).1.unwrap().holder,
                fresh.holder
            );
        });
    }

    #[test]
    fn release_waits_for_recovery_and_preserves_a_replacement_holder() {
        let temp = TempDir::new().unwrap();
        with_claims_root(temp.path(), || {
            let key = "node:release-race-test";
            let mut old = match claims::acquire(
                key,
                "target-session:old",
                claims::AcquireOpts {
                    pid: Some(reaped_pid()),
                    ..Default::default()
                },
            ) {
                claims::AcquireOutcome::Acquired(record) => record,
                other => panic!("claim fixture failed: {other:?}"),
            };
            let path = claims::claim_path(key, Some(temp.path())).unwrap();
            old.session_id = None;
            std::fs::write(&path, claims::serialize_claim(&old).unwrap()).unwrap();
            let lock = claims::recovery_lock_path(&path);
            let token = claims::acquire_dir_mutex(&lock, Duration::from_secs(2), true).unwrap();
            let root = temp.path().to_path_buf();
            let release_key = key.to_string();
            let holder = old.holder.clone();
            let release = thread::spawn(move || {
                claims::release(&release_key, &holder, Some(&root), Some(&root))
            });
            thread::sleep(Duration::from_millis(200));
            let waited_for_lock = !release.is_finished();

            let fresh = live_replacement(&old, "target-session:fresh");
            std::fs::write(&path, claims::serialize_claim(&fresh).unwrap()).unwrap();
            claims::release_dir_mutex(&lock, &token);

            release.join().unwrap().unwrap();
            assert!(
                waited_for_lock,
                "release ignored the per-key recovery mutex"
            );
            assert_eq!(
                claims::status(key, Some(temp.path())).1.unwrap().holder,
                fresh.holder
            );
        });
    }

    #[test]
    fn release_receipt_returns_only_the_claim_it_unlinked() {
        let temp = TempDir::new().unwrap();
        with_claims_root(temp.path(), || {
            let key = "node:release-receipt-test";
            let root = Some(temp.path().to_path_buf());
            let acquired = match claims::acquire(
                key,
                "target-session:owner",
                claims::AcquireOpts {
                    pid: Some(std::process::id()),
                    root: root.clone(),
                    ..Default::default()
                },
            ) {
                claims::AcquireOutcome::Acquired(record) => record,
                other => panic!("claim fixture failed: {other:?}"),
            };

            let released =
                claims::release_with_receipt(key, &acquired.holder, Some(temp.path()), None)
                    .unwrap()
                    .unwrap();
            assert_eq!(released.acquired_at, acquired.acquired_at);
            assert!(
                claims::release_with_receipt(key, &acquired.holder, Some(temp.path()), None,)
                    .unwrap()
                    .is_none()
            );

            let replacement = match claims::acquire(
                key,
                "target-session:replacement",
                claims::AcquireOpts {
                    pid: Some(std::process::id()),
                    root,
                    ..Default::default()
                },
            ) {
                claims::AcquireOutcome::Acquired(record) => record,
                other => panic!("replacement fixture failed: {other:?}"),
            };
            assert!(claims::release_with_receipt(
                key,
                "target-session:foreign",
                Some(temp.path()),
                None,
            )
            .unwrap()
            .is_none());
            assert_eq!(
                claims::status(key, Some(temp.path())).1.unwrap().holder,
                replacement.holder
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn strict_claim_listing_refuses_symlinked_lockfiles() {
        let temp = TempDir::new().unwrap();
        let path = claims::claim_path("node:x-symlink", Some(temp.path())).unwrap();
        let directory = path.parent().unwrap();
        std::fs::create_dir_all(directory).unwrap();
        let target = temp.path().join("claim-target");
        std::fs::write(&target, "not a lockfile").unwrap();
        std::os::unix::fs::symlink(target, &path).unwrap();

        let error =
            claims::list_in_strict(&[directory.to_path_buf()], Some("node:"), true).unwrap_err();
        assert!(error.contains("not a regular file"), "{error}");
    }

    #[test]
    fn strict_claim_listing_refuses_a_lockfile_with_a_mismatched_key() {
        let temp = TempDir::new().unwrap();
        let root = Some(temp.path().to_path_buf());
        let record = match claims::acquire(
            "node:x-original",
            "target-session:owner",
            claims::AcquireOpts {
                pid: Some(std::process::id()),
                root: root.clone(),
                ..Default::default()
            },
        ) {
            claims::AcquireOutcome::Acquired(record) => record,
            other => panic!("claim fixture failed: {other:?}"),
        };
        let wrong_path = claims::claim_path("node:x-mismatch", Some(temp.path())).unwrap();
        std::fs::write(&wrong_path, claims::serialize_claim(&record).unwrap()).unwrap();
        let directory = wrong_path.parent().unwrap();

        let error =
            claims::list_in_strict(&[directory.to_path_buf()], Some("node:"), true).unwrap_err();
        assert!(error.contains("filename does not match key"), "{error}");
    }

    #[test]
    fn strict_node_listing_refuses_a_node_filename_with_a_non_node_claim() {
        let temp = TempDir::new().unwrap();
        let record = match claims::acquire(
            "task:x-original:1.1",
            "target-session:owner",
            claims::AcquireOpts {
                pid: Some(std::process::id()),
                root: Some(temp.path().to_path_buf()),
                ..Default::default()
            },
        ) {
            claims::AcquireOutcome::Acquired(record) => record,
            other => panic!("claim fixture failed: {other:?}"),
        };
        let node_path = claims::claim_path("node:x-mismatch", Some(temp.path())).unwrap();
        std::fs::write(&node_path, claims::serialize_claim(&record).unwrap()).unwrap();

        let error = claims::list_in_strict(
            &[node_path.parent().unwrap().to_path_buf()],
            Some("node:"),
            true,
        )
        .unwrap_err();
        assert!(error.contains("filename does not match key"), "{error}");
    }

    #[test]
    fn task_acquire_keeps_an_expired_claim_when_its_session_is_live() {
        let temp = TempDir::new().unwrap();
        with_claims_root(temp.path(), || {
            let key = "task:x-session-witness:1.1";
            let root = Some(temp.path().to_path_buf());
            let mut old = match claims::acquire(
                key,
                "target-session:thread-holder",
                claims::AcquireOpts {
                    pid_unavailable: true,
                    ttl_ms: Some(60_000),
                    root: root.clone(),
                    ..Default::default()
                },
            ) {
                claims::AcquireOutcome::Acquired(record) => record,
                other => panic!("claim fixture failed: {other:?}"),
            };
            let now = claims::now_ms();
            old.acquired_at = now - claims::UNRESOLVED_GRACE_MS - 120_000;
            old.expires_at = Some(now - claims::UNRESOLVED_GRACE_MS - 60_000);
            old.pid_provenance = Some("ambient".into());
            old.session_id = Some("thread-session".into());
            let path = claims::claim_path(key, Some(temp.path())).unwrap();
            std::fs::write(&path, claims::serialize_claim(&old).unwrap()).unwrap();

            let observations = std::cell::Cell::new(0usize);
            let inconsistent_witness = |_: &claims::ClaimRecord| {
                let call = observations.get();
                observations.set(call + 1);
                if call == 0 {
                    claims::SessionLiveness::Live("test-session-live")
                } else {
                    claims::SessionLiveness::Absent
                }
            };
            assert_eq!(
                claims::classify_with_session_witness(&old, Some(&inconsistent_witness),),
                claims::ClaimState::Live,
                "one classification must reuse its session witness answer"
            );
            assert_eq!(observations.get(), 1, "session witness was read twice");

            let live = |_: &claims::ClaimRecord| claims::SessionLiveness::Live("test-session-live");
            let live_witness: claims::SessionWitness<'_> = &live;
            let outcome = claims::acquire_with_session_witness(
                key,
                "target-session:second",
                claims::AcquireOpts {
                    pid: Some(std::process::id()),
                    root: root.clone(),
                    ..Default::default()
                },
                Some(live_witness),
            );
            assert!(
                matches!(&outcome, claims::AcquireOutcome::HeldByOther { holder, .. } if holder == &old.holder),
                "live thread claim was stolen: {outcome:?}"
            );

            old.expires_at = Some(now + 60_000);
            std::fs::write(&path, claims::serialize_claim(&old).unwrap()).unwrap();
            let absent = |_: &claims::ClaimRecord| claims::SessionLiveness::Absent;
            let absent_witness: claims::SessionWitness<'_> = &absent;
            let outcome = claims::acquire_with_session_witness(
                key,
                "target-session:second",
                claims::AcquireOpts {
                    pid: Some(std::process::id()),
                    root: root.clone(),
                    ..Default::default()
                },
                Some(absent_witness),
            );
            assert!(
                matches!(outcome, claims::AcquireOutcome::Acquired(_)),
                "absent thread claim stayed held: {outcome:?}"
            );

            let race_key = "task:x-session-witness:1.3";
            let mut raced_claim = match claims::acquire(
                race_key,
                "target-session:thread-holder",
                claims::AcquireOpts {
                    pid_unavailable: true,
                    ttl_ms: Some(60_000),
                    root: root.clone(),
                    ..Default::default()
                },
            ) {
                claims::AcquireOutcome::Acquired(record) => record,
                other => panic!("claim fixture failed: {other:?}"),
            };
            raced_claim.acquired_at = now - 120_000;
            raced_claim.expires_at = Some(now - 60_000);
            raced_claim.pid_provenance = Some("ambient".into());
            raced_claim.session_id = Some("thread-session-race".into());
            let race_path = claims::claim_path(race_key, Some(temp.path())).unwrap();
            std::fs::write(&race_path, claims::serialize_claim(&raced_claim).unwrap()).unwrap();

            let observations = std::cell::Cell::new(0usize);
            let becomes_live = |_: &claims::ClaimRecord| {
                let count = observations.get();
                observations.set(count + 1);
                if count == 0 {
                    claims::SessionLiveness::Absent
                } else {
                    claims::SessionLiveness::Live("test-session-live")
                }
            };
            let witness: claims::SessionWitness<'_> = &becomes_live;
            let outcome = claims::acquire_with_session_witness(
                race_key,
                "target-session:second",
                claims::AcquireOpts {
                    pid: Some(std::process::id()),
                    root,
                    ..Default::default()
                },
                Some(witness),
            );
            assert!(
                matches!(&outcome, claims::AcquireOutcome::HeldByOther { holder, .. } if holder == &raced_claim.holder),
                "newly live thread claim was stolen: {outcome:?}"
            );
            assert!(
                observations.get() >= 2,
                "acquire did not recheck under lock"
            );
        });
    }

    #[test]
    fn task_reap_rechecks_session_liveness_under_recovery_lock() {
        let temp = TempDir::new().unwrap();
        with_claims_root(temp.path(), || {
            let key = "task:x-session-witness:1.2";
            let root = Some(temp.path().to_path_buf());
            let mut old = match claims::acquire(
                key,
                "target-session:thread-holder",
                claims::AcquireOpts {
                    pid_unavailable: true,
                    ttl_ms: Some(60_000),
                    root: root.clone(),
                    ..Default::default()
                },
            ) {
                claims::AcquireOutcome::Acquired(record) => record,
                other => panic!("claim fixture failed: {other:?}"),
            };
            let now = claims::now_ms();
            old.acquired_at = now - 120_000;
            old.expires_at = Some(now - 60_000);
            old.pid_provenance = Some("ambient".into());
            old.session_id = Some("thread-session-reap".into());
            let path = claims::claim_path(key, Some(temp.path())).unwrap();
            std::fs::write(&path, claims::serialize_claim(&old).unwrap()).unwrap();

            let observations = std::cell::Cell::new(0usize);
            let becomes_live = |_: &claims::ClaimRecord| {
                let count = observations.get();
                observations.set(count + 1);
                if count == 0 {
                    claims::SessionLiveness::Absent
                } else {
                    claims::SessionLiveness::Live("test-session-live")
                }
            };
            let witness: claims::SessionWitness<'_> = &becomes_live;
            let result =
                reap_with_session_witness(root.as_deref(), true, Some(witness), Some(witness))
                    .unwrap();
            assert_eq!(
                result["reaped"], 0,
                "live thread claim was reaped: {result}"
            );
            assert!(path.exists(), "live thread claim file was archived");
            assert!(observations.get() >= 2, "reaper did not recheck under lock");
        });
    }
}
