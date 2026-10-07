//! Claim ownership in graph.db. Legacy files are read only during migration.

use crate::claims::{self, AcquireOpts, AcquireOutcome, ClaimRecord, ClaimState, SessionWitness};
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

const CLOCK: &str = "CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER)";
const COLUMNS: &str = "key, holder, schema_version, acquired_at, expires_at, pid, pid_unavailable, host, machine_id, reason, harness, session_id, pid_provenance, metadata";

pub(crate) fn database_path(root: Option<&Path>) -> Result<PathBuf, String> {
    database_path_from_directory(&directory(root)?)
}

fn directory(root: Option<&Path>) -> Result<PathBuf, String> {
    claims::claims_dir_for(root).ok_or_else(|| "claims root is unavailable".into())
}

pub(crate) fn database_path_from_directory(dir: &Path) -> Result<PathBuf, String> {
    let state = dir
        .parent()
        .ok_or_else(|| format!("{} has no state root", dir.display()))?;
    Ok(crate::state_layout::place(state, "graph.json").with_extension("db"))
}

pub fn open(root: Option<&Path>) -> Result<Connection, String> {
    open_directory(&directory(root)?)
}

fn open_for_key(key: &str, root: Option<&Path>) -> Result<Connection, String> {
    open_directory(&crate::claims_root::claims_dir(key, root)?)
}

pub(crate) fn open_directory(dir: &Path) -> Result<Connection, String> {
    let path = database_path_from_directory(dir)?;
    crate::state_layout_sqlite::wait_for_fence(dir.parent().unwrap());
    let mut connection = crate::store_conn::open_write(&path)?;
    connection
        .execute_batch(DDL)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    connection.execute_batch("CREATE TABLE IF NOT EXISTS claim_history (id INTEGER PRIMARY KEY, retired_at INTEGER NOT NULL, record TEXT NOT NULL);
        CREATE TRIGGER IF NOT EXISTS claims_archive_delete BEFORE DELETE ON claims BEGIN
            INSERT INTO claim_history(retired_at, record) VALUES (CAST((julianday('now')-2440587.5)*86400000 AS INTEGER), json_object('key',old.key,'holder',old.holder,'schema_version',old.schema_version,'acquired_at',old.acquired_at,'expires_at',old.expires_at,'pid',old.pid,'pid_unavailable',json(CASE WHEN old.pid_unavailable THEN 'true' ELSE 'false' END),'host',old.host,'machine_id',old.machine_id,'reason',old.reason,'harness',old.harness,'session_id',old.session_id,'pid_provenance',old.pid_provenance,'metadata',json(old.metadata)));
        END;").map_err(|e| e.to_string())?;
    import_lockfiles(&mut connection, dir)?;
    Ok(connection)
}

fn import_lockfiles(connection: &mut Connection, dir: &Path) -> Result<(), String> {
    let imported: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM claim_meta WHERE key = 'table_authority_v1')",
            [],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    if imported {
        return Ok(());
    }
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| e.to_string())?;
    let imported: bool = transaction
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM claim_meta WHERE key = 'table_authority_v1')",
            [],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    if imported {
        return Ok(());
    }
    let source = retire_directory(dir)?;
    migrate_auxiliary(dir, source.as_deref())?;
    let records = match source.as_deref() {
        Some(source) => read_legacy_directory(source),
        None => Ok(Vec::new()),
    }?;
    // The old table was an unused projection of the files. It cannot win over
    // the final file snapshot at the authority transition.
    transaction
        .execute("DELETE FROM claims", [])
        .map_err(|e| e.to_string())?;
    for record in records {
        insert_record(&transaction, &record)?;
    }
    transaction.execute("INSERT OR REPLACE INTO claim_meta (key, value) VALUES ('lockfiles_imported', '1'), ('table_authority_v1', '1')", []).map_err(|e| e.to_string())?;
    transaction.commit().map_err(|e| e.to_string())
}

fn read_legacy_directory(dir: &Path) -> Result<Vec<ClaimRecord>, String> {
    let mut records = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".lock") {
            continue;
        }
        if !entry.file_type().map_err(|e| e.to_string())?.is_file() {
            return Err(format!(
                "{} is not a regular claim file",
                entry.path().display()
            ));
        }
        let record = claims::read_legacy_claim_file(&entry.path())
            .map_err(|e| format!("{}: {e:?}", entry.path().display()))?;
        if name != format!("{}.lock", claims::encode_key(&record.key)) {
            return Err(format!(
                "{} filename does not match key {}",
                entry.path().display(),
                record.key
            ));
        }
        records.push(record);
    }
    Ok(records)
}

fn retire_directory(dir: &Path) -> Result<Option<PathBuf>, String> {
    use std::io::Write;
    let parent = dir
        .parent()
        .ok_or_else(|| "claims directory has no parent".to_string())?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let metadata = match std::fs::symlink_metadata(dir) {
        Ok(metadata) => Some(metadata),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.to_string()),
    };
    if metadata.as_ref().is_some_and(|m| m.file_type().is_file()) {
        let marker: Value = serde_json::from_slice(&std::fs::read(dir).map_err(|e| e.to_string())?)
            .map_err(|e| format!("{}: invalid claims migration marker: {e}", dir.display()))?;
        if marker.get("storage") != Some(&json!("graph.db")) {
            return Err(format!(
                "{} is not a claims migration marker",
                dir.display()
            ));
        }
        return Ok(marker
            .get("source")
            .and_then(Value::as_str)
            .map(PathBuf::from));
    }
    if metadata
        .as_ref()
        .is_some_and(|m| !m.is_dir() || m.file_type().is_symlink())
    {
        return Err(format!(
            "{} is not a regular claims directory",
            dir.display()
        ));
    }
    let backup = parent.join("backups/claims-table");
    std::fs::create_dir_all(&backup).map_err(|e| e.to_string())?;
    if metadata.is_none() {
        if let Some((temporary, source)) = interrupted_handoff(&backup)? {
            // A crash fell between retiring the directory and publishing the
            // marker. Finish the handoff so the archived claims still import.
            std::fs::rename(&temporary, dir).map_err(|e| e.to_string())?;
            return Ok(Some(source));
        }
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let source = metadata
        .as_ref()
        .map(|_| backup.join(format!("claims-{stamp}-{}", std::process::id())));
    let temporary = backup.join(format!("marker-{stamp}-{}", std::process::id()));
    let marker = json!({"storage":"graph.db", "source":source, "remedy":"upgrade fno; claims are stored in graph.db"});
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .map_err(|e| e.to_string())?;
    if let Err(error) = file
        .write_all(
            serde_json::to_string(&marker)
                .map_err(|e| e.to_string())?
                .as_bytes(),
        )
        .and_then(|()| file.sync_all())
    {
        let _ = std::fs::remove_file(&temporary);
        return Err(error.to_string());
    }
    drop(file);
    if let Some(source) = &source {
        if let Err(error) = read_legacy_directory(dir) {
            let _ = std::fs::remove_file(&temporary);
            return Err(error);
        }
        // Legacy writers publish at the original path. Once it is retired,
        // their final rename or link cannot reach the archived snapshot.
        if let Err(error) = std::fs::rename(dir, source) {
            let _ = std::fs::remove_file(&temporary);
            return Err(format!(
                "claims migration could not retire {} to {}: {error}",
                dir.display(),
                source.display()
            ));
        }
    }
    if let Err(error) = std::fs::rename(&temporary, dir) {
        if let Some(source) = &source {
            std::fs::rename(source, dir).map_err(|restore| {
                format!(
                    "claims migration refused: {error}; source retained at {}: {restore}",
                    source.display()
                )
            })?;
        }
        let _ = std::fs::remove_file(&temporary);
        return Err(format!(
            "{}: claims migration fence failed: {error}",
            dir.display()
        ));
    }
    Ok(source)
}

fn interrupted_handoff(backup: &Path) -> Result<Option<(PathBuf, PathBuf)>, String> {
    for entry in std::fs::read_dir(backup).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if !path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("marker-"))
        {
            continue;
        }
        let Ok(marker) =
            serde_json::from_slice::<Value>(&std::fs::read(&path).map_err(|e| e.to_string())?)
        else {
            continue;
        };
        if let Some(source) = marker
            .get("source")
            .and_then(Value::as_str)
            .map(PathBuf::from)
        {
            if source.is_dir() {
                return Ok(Some((path, source)));
            }
        }
    }
    Ok(None)
}

fn migrate_auxiliary(dir: &Path, source: Option<&Path>) -> Result<(), String> {
    if let Some(source) = source {
        let auxiliary = crate::claims_root::auxiliary_dir(dir);
        for entry in std::fs::read_dir(source).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name();
            let text = name.to_string_lossy();
            if text.ends_with(".held-requests")
                || text.ends_with(".queue.d")
                || text.ends_with(".priority.d")
                || text.ends_with(".full.d")
                || text == "build-waiters"
            {
                std::fs::create_dir_all(&auxiliary).map_err(|e| e.to_string())?;
                let target = auxiliary.join(&name);
                if target.exists() {
                    continue;
                }
                std::fs::rename(entry.path(), &target)
                    .map_err(|e| format!("claims auxiliary migration {}: {e}", target.display()))?;
            }
        }
    }
    Ok(())
}

fn insert_record(connection: &Connection, record: &ClaimRecord) -> Result<(), String> {
    claims::validate_record(record)?;
    connection.execute("INSERT INTO claims (key, holder, schema_version, acquired_at, expires_at, pid, pid_unavailable, host, machine_id, reason, harness, session_id, pid_provenance, metadata) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)", params![record.key, record.holder, record.schema_version, record.acquired_at, record.expires_at, record.pid, record.pid_unavailable, record.host, record.machine_id, record.reason, record.harness, record.session_id, record.pid_provenance, serde_json::to_string(&record.metadata).map_err(|e| e.to_string())?]).map_err(|e| e.to_string())?;
    Ok(())
}

/// Test seam: seed a claim written as the old lockfile YAML.
#[cfg(test)]
pub(crate) fn seed_yaml_at_path(path: &Path, yaml: &str) {
    seed_at_path(path, &serde_yaml_ng::from_str(yaml).unwrap());
}

/// Test seam: put `record` in the table behind a claim locator path.
#[cfg(test)]
pub(crate) fn seed_at_path(path: &Path, record: &ClaimRecord) {
    let connection = open_directory(path.parent().unwrap()).unwrap();
    connection
        .execute("DELETE FROM claims WHERE key = ?1", [&record.key])
        .unwrap();
    insert_record(&connection, record).unwrap();
}

fn decode(row: &rusqlite::Row<'_>) -> rusqlite::Result<ClaimRecord> {
    let metadata: String = row.get(13)?;
    let metadata = serde_json::from_str(&metadata).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(13, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let record = ClaimRecord {
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
        metadata,
    };
    claims::validate_record(&record).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            std::io::Error::new(std::io::ErrorKind::InvalidData, error).into(),
        )
    })?;
    Ok(record)
}

fn record_for(connection: &Connection, key: &str) -> Result<Option<ClaimRecord>, String> {
    connection
        .query_row(
            &format!("SELECT {COLUMNS} FROM claims WHERE key = ?1"),
            [key],
            decode,
        )
        .optional()
        .map_err(|e| e.to_string())
}

pub(crate) fn read(key: &str, root: Option<&Path>) -> Result<Option<ClaimRecord>, String> {
    record_for(&open_for_key(key, root)?, key)
}

pub(crate) fn read_at_path(path: &Path) -> Result<Option<ClaimRecord>, String> {
    let dir = path
        .parent()
        .ok_or_else(|| "claim locator has no parent".to_string())?;
    let name = path
        .file_name()
        .and_then(|v| v.to_str())
        .and_then(|v| v.strip_suffix(".lock"))
        .ok_or_else(|| "claim locator has no encoded key".to_string())?;
    let mut decoded = Vec::new();
    let bytes = name.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let pair = name
                .get(at + 1..at + 3)
                .ok_or_else(|| "invalid encoded claim key".to_string())?;
            decoded.push(u8::from_str_radix(pair, 16).map_err(|e| e.to_string())?);
            at += 3;
        } else {
            decoded.push(bytes[at]);
            at += 1;
        }
    }
    let key = String::from_utf8(decoded).map_err(|e| e.to_string())?;
    if claims::encode_key(&key) != name {
        return Err("claim locator key is not canonical".into());
    }
    record_for(&open_directory(dir)?, &key).map_err(|e| format!("Corrupted({e})"))
}

pub(crate) fn records_in(
    dir: &Path,
    prefix: Option<&str>,
    include_stale: bool,
) -> Result<Vec<ClaimRecord>, String> {
    let connection = open_directory(dir)?;
    let mut statement = connection
        .prepare(&format!("SELECT {COLUMNS} FROM claims ORDER BY key"))
        .map_err(|e| e.to_string())?;
    let rows = statement.query_map([], decode).map_err(|e| e.to_string())?;
    let mut records = Vec::new();
    for row in rows {
        // One unreadable row must not blind the whole scan; `claim status`
        // on its key still reports it corrupted.
        let Ok(record) = row else {
            continue;
        };
        if prefix.is_some_and(|p| !record.key.starts_with(p)) {
            continue;
        }
        if !include_stale
            && !matches!(
                crate::claim_verbs::status_verdict(&record).0,
                ClaimState::Live | ClaimState::Suspect
            )
        {
            continue;
        }
        records.push(record);
    }
    Ok(records)
}

pub(crate) fn acquire(
    key: &str,
    holder: &str,
    options: &AcquireOpts,
    witness: Option<SessionWitness<'_>>,
) -> Result<AcquireOutcome, String> {
    claims::validate_inputs(
        key,
        holder,
        options.ttl_ms,
        options.pid,
        options.pid_unavailable,
    )?;
    let connection = open_for_key(key, options.root.as_deref())?;
    let observed = record_for(&connection, key)?;
    // Takeover is a compare-and-swap on the row we classified. The clock
    // alone never decides: an expired lease whose pid or session is live stays held.
    let local_dead = observed.as_ref().filter(|r| {
        !matches!(
            claims::classify_with_session_witness(r, witness),
            ClaimState::Live | ClaimState::Suspect
        )
    });
    let record = claims::make_claim(key, holder, options);
    claims::validate_record(&record)?;
    let sql = format!("INSERT INTO claims ({COLUMNS}) VALUES (?1,?2,?3,{CLOCK},CASE WHEN ?4 IS NULL THEN NULL ELSE {CLOCK}+?4 END,?5,?6,?7,?8,?9,?10,?11,?12,?13)
        ON CONFLICT(key) DO UPDATE SET holder=excluded.holder, schema_version=excluded.schema_version,
        acquired_at=MAX(excluded.acquired_at,claims.acquired_at+1), expires_at=excluded.expires_at,
        pid=excluded.pid,pid_unavailable=excluded.pid_unavailable,host=excluded.host,machine_id=excluded.machine_id,
        reason=excluded.reason,harness=excluded.harness,session_id=excluded.session_id,pid_provenance=excluded.pid_provenance,metadata=excluded.metadata
        WHERE claims.holder=excluded.holder
        OR (claims.holder IS ?14 AND claims.acquired_at IS ?15 AND claims.expires_at IS ?16)
        RETURNING {COLUMNS}");
    let result = connection
        .query_row(
            &sql,
            params![
                key,
                holder,
                record.schema_version,
                options.ttl_ms,
                record.pid,
                record.pid_unavailable,
                record.host,
                record.machine_id,
                record.reason,
                record.harness,
                record.session_id,
                record.pid_provenance,
                serde_json::to_string(&record.metadata).map_err(|e| e.to_string())?,
                local_dead.map(|r| r.holder.as_str()),
                local_dead.map(|r| r.acquired_at),
                local_dead.and_then(|r| r.expires_at)
            ],
            decode,
        )
        .optional()
        .map_err(|e| e.to_string())?;
    match result {
        Some(record) => {
            let mut data = claims::common_event_data(&record);
            let event = if observed.as_ref().is_some_and(|r| r.holder == holder) {
                "claim_idempotent_reacquired"
            } else if observed.is_some() {
                "claim_stale_reclaimed"
            } else {
                "claim_acquired"
            };
            if let Some(previous) = &observed {
                data.insert("previous_acquired_at".into(), json!(previous.acquired_at));
                data.insert("previous_holder".into(), json!(previous.holder));
                data.insert("previous_pid".into(), json!(previous.pid));
            }
            if let Some(reason) = &record.reason {
                data.insert("reason".into(), json!(reason));
            }
            claims::emit_audit_event(options.events_dir.as_deref(), event, data);
            Ok(AcquireOutcome::Acquired(record))
        }
        None => {
            let existing = record_for(&connection, key)?
                .ok_or_else(|| "claim changed after refused acquire; retry".to_string())?;
            Ok(AcquireOutcome::HeldByOther {
                holder: existing.holder,
                pid: existing.pid,
                host: existing.host,
            })
        }
    }
}

pub(crate) fn replace_observed_at(
    path: &Path,
    observed: &ClaimRecord,
    next: &ClaimRecord,
) -> Result<Option<ClaimRecord>, String> {
    claims::validate_record(next)?;
    let connection = open_directory(
        path.parent()
            .ok_or_else(|| "claim locator has no parent".to_string())?,
    )?;
    connection.query_row(&format!("UPDATE claims SET holder=?4,schema_version=?5,acquired_at=?6,expires_at=?7,pid=?8,pid_unavailable=?9,host=?10,machine_id=?11,reason=?12,harness=?13,session_id=?14,pid_provenance=?15,metadata=?16
        WHERE key=?1 AND holder=?2 AND acquired_at=?3 AND expires_at IS ?17 AND pid IS ?18
        AND schema_version=?19 AND pid_unavailable=?20 AND host=?21 AND machine_id IS ?22 AND reason IS ?23 AND harness IS ?24 AND session_id IS ?25 AND pid_provenance IS ?26 AND metadata=?27 RETURNING {COLUMNS}"),
        params![observed.key,observed.holder,observed.acquired_at,next.holder,next.schema_version,next.acquired_at,next.expires_at,next.pid,next.pid_unavailable,next.host,next.machine_id,next.reason,next.harness,next.session_id,next.pid_provenance,serde_json::to_string(&next.metadata).map_err(|e| e.to_string())?,observed.expires_at,observed.pid,observed.schema_version,observed.pid_unavailable,observed.host,observed.machine_id,observed.reason,observed.harness,observed.session_id,observed.pid_provenance,serde_json::to_string(&observed.metadata).map_err(|e| e.to_string())?], decode).optional().map_err(|e| e.to_string())
}

pub(crate) fn release(
    key: &str,
    holder: &str,
    root: Option<&Path>,
    events: Option<&Path>,
) -> Result<Option<ClaimRecord>, String> {
    let connection = open_for_key(key, root)?;
    let removed = connection
        .query_row(
            &format!("DELETE FROM claims WHERE key=?1 AND holder=?2 RETURNING {COLUMNS}"),
            params![key, holder],
            decode,
        )
        .optional()
        .map_err(|e| e.to_string())?;
    if let Some(record) = &removed {
        let mut data = claims::common_event_data(record);
        data.insert(
            "duration_held_ms".into(),
            json!((claims::now_ms() - record.acquired_at).max(0)),
        );
        claims::emit_audit_event(events, "claim_released", data);
    }
    Ok(removed)
}

pub(crate) fn delete_observed(dir: &Path, record: &ClaimRecord) -> Result<bool, String> {
    open_directory(dir)?.execute("DELETE FROM claims WHERE key=?1 AND holder=?2 AND acquired_at=?3 AND expires_at IS ?4 AND pid IS ?5 AND schema_version=?6 AND pid_unavailable=?7 AND host=?8 AND machine_id IS ?9 AND reason IS ?10 AND harness IS ?11 AND session_id IS ?12 AND pid_provenance IS ?13 AND metadata=?14",
        params![record.key,record.holder,record.acquired_at,record.expires_at,record.pid,record.schema_version,record.pid_unavailable,record.host,record.machine_id,record.reason,record.harness,record.session_id,record.pid_provenance,serde_json::to_string(&record.metadata).map_err(|e| e.to_string())?]).map(|n| n==1).map_err(|e| e.to_string())
}

pub(crate) fn renew(
    key: &str,
    holder: &str,
    ttl: i64,
    root: Option<&Path>,
) -> Result<bool, String> {
    if key.is_empty() || holder.is_empty() || ttl <= 0 {
        return Err("key, holder and positive ttl_ms are required".into());
    }
    let connection = open_for_key(key, root)?;
    let Some(observed) = record_for(&connection, key)? else {
        return Ok(false);
    };
    if observed.holder != holder || observed.expires_at.is_none() {
        return Ok(false);
    }
    // The verdict, not the clock, refuses: an expired lease whose holder
    // reads live or suspect extends, exactly as `claim status` reports it.
    // The verdict runs only once expired, so a routine renewal skips its probes.
    if observed.expires_at.is_some_and(|at| at <= claims::now_ms())
        && crate::claim_verbs::status_verdict(&observed).0 == ClaimState::Stale
    {
        return Ok(false);
    }
    let next = claims::renewed_record(&observed, ttl);
    let sql = format!("UPDATE claims SET expires_at={CLOCK}+?4,pid=?6,host=?7,machine_id=?8,session_id=?9 WHERE key=?1 AND holder=?2 AND acquired_at=?3 AND expires_at IS ?5 AND pid IS ?10");
    Ok(connection
        .execute(
            &sql,
            params![
                key,
                holder,
                observed.acquired_at,
                ttl,
                observed.expires_at,
                next.pid,
                next.host,
                next.machine_id,
                next.session_id,
                observed.pid
            ],
        )
        .map_err(|e| e.to_string())?
        == 1)
}

pub fn list_db(
    prefix: Option<&str>,
    include_stale: bool,
    root: Option<&Path>,
) -> Result<Value, String> {
    let records = records_in(&directory(root)?, prefix, include_stale)?;
    Ok(
        json!({"rows": records.into_iter().map(|r| crate::claim_verbs::claim_status_value(&r)).collect::<Vec<_>>() }),
    )
}

pub fn list_repo_space(prefix: &str, include_stale: bool) -> Result<Vec<ClaimRecord>, String> {
    records_in(
        &crate::claims_root::claims_dir(&format!("{prefix}probe"), None)?,
        Some(prefix),
        include_stale,
    )
}

pub fn force_release(
    key: &str,
    reason: &str,
    root: Option<&Path>,
    _holding_recovery_lock: bool,
) -> Result<Value, String> {
    if key.is_empty() || reason.trim().is_empty() {
        return Err("key and override reason must be non-empty".into());
    }
    let connection = open_for_key(key, root)?;
    // Raw columns, not a decoded record: the force path is the one way out
    // for a row that no longer validates.
    let removed: Option<(Option<String>, Option<i64>)> = connection
        .query_row(
            "DELETE FROM claims WHERE key=?1 RETURNING holder, pid",
            [key],
            |r| Ok((r.get(0).ok(), r.get(1).ok().flatten())),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    let (holder, pid) = removed.clone().unwrap_or_default();
    let mut data = serde_json::Map::new();
    data.insert("key".into(), json!(key));
    data.insert("override_reason".into(), json!(reason));
    if removed.is_some() {
        data.insert("previous_holder".into(), json!(holder));
        data.insert("previous_pid".into(), json!(pid));
    }
    claims::emit_audit_event(None, "claim_force_overridden", data);
    Ok(
        json!({"key":key,"path":database_path(root)?,"archived":removed.is_some(),"force_released":removed.is_some(),"previous_holder":holder,"previous_pid":pid}),
    )
}

pub(crate) fn force_release_observed(
    key: &str,
    reason: &str,
    root: Option<&Path>,
    expected: &ClaimRecord,
) -> Result<Value, String> {
    if key != expected.key || reason.trim().is_empty() {
        return Err("expected claim key and override reason must match".into());
    }
    let dir = crate::claims_root::claims_dir(key, root)?;
    force_release_observed_in_directory(&dir, key, reason, expected)
}

pub(crate) fn force_release_observed_in_directory(
    dir: &Path,
    key: &str,
    reason: &str,
    expected: &ClaimRecord,
) -> Result<Value, String> {
    if key != expected.key || reason.trim().is_empty() {
        return Err("expected claim key and override reason must match".into());
    }
    let removed = delete_observed(dir, expected)?;
    if removed {
        let mut data = claims::common_event_data(expected);
        data.insert("override_reason".into(), json!(reason));
        claims::emit_audit_event(None, "claim_force_overridden", data);
    }
    Ok(
        json!({"key":key,"path":database_path_from_directory(&dir)?,"archived":removed,"force_released":removed,"previous_holder":if removed {Some(&expected.holder)} else {None}}),
    )
}

pub fn reap(root: Option<&Path>, apply: bool) -> Result<Value, String> {
    reap_with_session_witness(root, apply, None, None)
}

pub(crate) fn reap_with_session_witness(
    root: Option<&Path>,
    apply: bool,
    witness: Option<SessionWitness<'_>>,
    recheck: Option<SessionWitness<'_>>,
) -> Result<Value, String> {
    reap_in_directory(&directory(root)?, apply, witness, recheck, None)
}

pub(crate) fn reap_in_directory(
    dir: &Path,
    apply: bool,
    witness: Option<SessionWitness<'_>>,
    recheck: Option<SessionWitness<'_>>,
    key: Option<&str>,
) -> Result<Value, String> {
    let records = records_in(dir, key, true)?
        .into_iter()
        .filter(|r| key.is_none_or(|k| r.key == k))
        .collect::<Vec<_>>();
    let mut would_reap = 0;
    let mut reaped = 0;
    let mut failures = Vec::new();
    for record in &records {
        if !claims::is_same_machine(&record.host, record.machine_id.as_deref())
            || claims::classify_with_session_witness(record, witness) != ClaimState::Stale
        {
            continue;
        }
        would_reap += 1;
        if !apply
            || claims::classify_with_session_witness(record, recheck.or(witness))
                != ClaimState::Stale
        {
            continue;
        }
        match delete_observed(dir, record) {
            Ok(true) => reaped += 1,
            Ok(false) => {}
            Err(e) => failures.push(e),
        }
    }
    Ok(
        json!({"apply":apply,"scanned":records.len(),"would_reap":would_reap,"reaped":reaped,"reap_failed":failures,"root":dir}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reap_observes_codex_sessions_and_preserves_a_replaced_generation() {
        let root = tempfile::tempdir().unwrap();
        let key = "node:reap-generation";
        let record = match claims::acquire(
            key,
            "owner",
            AcquireOpts {
                root: Some(root.path().to_path_buf()),
                pid: Some(std::process::id()),
                ttl_ms: Some(60_000),
                identity: Some(("codex-thread".into(), "codex".into())),
                ..Default::default()
            },
        ) {
            AcquireOutcome::Acquired(record) => record,
            other => panic!("{other:?}"),
        };
        let unknown = |_: &ClaimRecord| claims::SessionLiveness::Unresolved;
        let kept =
            reap_with_session_witness(Some(root.path()), true, Some(&unknown), Some(&unknown))
                .unwrap();
        assert_eq!(kept["would_reap"], 0);
        let absent = |_: &ClaimRecord| claims::SessionLiveness::Absent;
        let path = claims::claim_path(key, Some(root.path())).unwrap();
        let replacement = |observed: &ClaimRecord| {
            let mut fresh = observed.clone();
            fresh.reason = Some("replacement".into());
            replace_observed_at(&path, observed, &fresh)
                .unwrap()
                .unwrap();
            claims::SessionLiveness::Absent
        };
        let raced =
            reap_with_session_witness(Some(root.path()), true, Some(&absent), Some(&replacement))
                .unwrap();
        assert_eq!(raced["would_reap"], 1);
        assert_eq!(raced["reaped"], 0);
        assert_eq!(
            read(key, Some(root.path()))
                .unwrap()
                .unwrap()
                .reason
                .as_deref(),
            Some("replacement")
        );
        let released =
            reap_with_session_witness(Some(root.path()), true, Some(&absent), Some(&absent))
                .unwrap();
        assert_eq!(released["reaped"], 1);
        assert!(read(key, Some(root.path())).unwrap().is_none());
        assert_eq!(record.holder, "owner");
    }
}
