//! Claim ownership in graph.db. Legacy files are read only during migration.
//!
//! With `store.remote_url` set, the keys in [`SHARED_PREFIXES`] live on the
//! shared primary instead (`store_remote`). Every statement here runs the
//! same SQL on either store, so the two cannot drift.

use crate::claims::{self, AcquireOpts, AcquireOutcome, ClaimRecord, ClaimState, SessionWitness};
use crate::store_remote::{Remote, SqlValue};
use rusqlite::{params, Connection, TransactionBehavior};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Claim keys that decide dispatch. With `store.remote_url` set they live on
/// the shared primary, so two machines never hold one node. Every other key
/// names a machine-local resource (`build:cargo`, `session:`, `worker:`) and
/// stays in this machine's graph.db.
pub(crate) const SHARED_PREFIXES: &[&str] = &["node:", "dispatch:", "reconcile:"];

pub(crate) fn shared(key: &str) -> bool {
    SHARED_PREFIXES.iter().any(|p| key.starts_with(p))
}

/// One claim store: this machine's graph.db, or the shared primary.
enum Db {
    Local(Connection),
    Remote(Remote),
}

impl Db {
    fn all(&self, sql: &str, args: &[SqlValue]) -> Result<Vec<Vec<SqlValue>>, String> {
        match self {
            Db::Remote(remote) => Ok(on_primary(remote, sql, args)?.rows),
            Db::Local(connection) => {
                let mut statement = connection.prepare(sql).map_err(|e| e.to_string())?;
                let width = statement.column_count();
                let mut rows = statement
                    .query(rusqlite::params_from_iter(args))
                    .map_err(|e| e.to_string())?;
                let mut out = Vec::new();
                while let Some(row) = rows.next().map_err(|e| e.to_string())? {
                    out.push(
                        (0..width)
                            .map(|i| row.get_ref(i).map(SqlValue::from))
                            .collect::<Result<_, _>>()
                            .map_err(|e| e.to_string())?,
                    );
                }
                Ok(out)
            }
        }
    }

    fn one(&self, sql: &str, args: &[SqlValue]) -> Result<Option<Vec<SqlValue>>, String> {
        Ok(self.all(sql, args)?.into_iter().next())
    }

    fn execute(&self, sql: &str, args: &[SqlValue]) -> Result<u64, String> {
        match self {
            Db::Remote(remote) => Ok(on_primary(remote, sql, args)?.affected),
            Db::Local(connection) => connection
                .execute(sql, rusqlite::params_from_iter(args))
                .map(|n| n as u64)
                .map_err(|e| e.to_string()),
        }
    }
}

#[cfg(test)]
thread_local! {
    static TEST_PRIMARY: std::cell::RefCell<Option<(Remote, PathBuf)>> =
        const { std::cell::RefCell::new(None) };
}

/// Test seam: route the shared keys under `dir` to `remote` on this thread.
#[cfg(test)]
pub(crate) fn route_to_primary(route: Option<(Remote, PathBuf)>) {
    TEST_PRIMARY.with(|p| *p.borrow_mut() = route);
}

/// The primary that holds the shared claims under `dir`. Only the global
/// claims directory is shared, so a sandbox or an explicit root never dials
/// out.
pub(crate) fn primary_for(dir: &Path) -> Result<Option<Remote>, String> {
    #[cfg(test)]
    if let Some((remote, at)) = TEST_PRIMARY.with(|p| p.borrow().clone()) {
        return Ok((at == dir).then_some(remote));
    }
    let Some(remote) = crate::store_remote::configured()? else {
        return Ok(None);
    };
    Ok((directory(None).ok().as_deref() == Some(dir)).then_some(remote))
}

/// One statement on the primary. The claim tables are made on first use: a
/// statement that meets no table runs the DDL and retries once, so a process
/// pays no extra round trip on a primary that already has them.
fn on_primary(
    remote: &Remote,
    sql: &str,
    args: &[SqlValue],
) -> Result<crate::store_remote::Reply, String> {
    match remote.execute(sql, args) {
        Err(error) if error.contains("no such table") => {
            // The primary never held lockfiles: its table is the authority
            // from birth. The stamp also keeps an exported copy's rows when it
            // opens as a local store, since the lockfile import clears an
            // unstamped table.
            remote.script(&format!(
                "{DDL}\n{}\nINSERT OR IGNORE INTO claim_meta (key, value) VALUES ('lockfiles_imported', '1'), ('table_authority_v1', '1');",
                history_ddl()
            ))?;
            remote.execute(sql, args)
        }
        reply => reply,
    }
}

fn db_in(dir: &Path, key: &str) -> Result<Db, String> {
    if shared(key) {
        if let Some(remote) = primary_for(dir)? {
            return Ok(Db::Remote(remote));
        }
    }
    open_directory(dir).map(Db::Local)
}

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

fn open_for_key(key: &str, root: Option<&Path>) -> Result<Db, String> {
    db_in(&crate::claims_root::claims_dir(key, root)?, key)
}

fn history_ddl() -> String {
    // A takeover rewrites the row in place, so a holder change archives the
    // old row just as a delete does.
    format!(
        "CREATE TABLE IF NOT EXISTS claim_history (id INTEGER PRIMARY KEY, retired_at INTEGER NOT NULL, record TEXT NOT NULL);
        CREATE TRIGGER IF NOT EXISTS claims_archive_delete BEFORE DELETE ON claims BEGIN {ARCHIVE_OLD} END;
        CREATE TRIGGER IF NOT EXISTS claims_archive_takeover BEFORE UPDATE OF holder ON claims WHEN old.holder IS NOT new.holder BEGIN {ARCHIVE_OLD} END;"
    )
}

const ARCHIVE_OLD: &str = "INSERT INTO claim_history(retired_at, record) VALUES (CAST((julianday('now')-2440587.5)*86400000 AS INTEGER), json_object('key',old.key,'holder',old.holder,'schema_version',old.schema_version,'acquired_at',old.acquired_at,'expires_at',old.expires_at,'pid',old.pid,'pid_unavailable',json(CASE WHEN old.pid_unavailable THEN 'true' ELSE 'false' END),'host',old.host,'machine_id',old.machine_id,'reason',old.reason,'harness',old.harness,'session_id',old.session_id,'pid_provenance',old.pid_provenance,'metadata',CASE WHEN json_valid(old.metadata) THEN json(old.metadata) ELSE old.metadata END));";

pub(crate) fn open_directory(dir: &Path) -> Result<Connection, String> {
    let path = database_path_from_directory(dir)?;
    crate::state_layout_sqlite::wait_for_fence(dir.parent().unwrap());
    let mut connection = crate::store_conn::open_write(&path)?;
    connection
        .execute_batch(DDL)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    connection
        .execute_batch(&history_ddl())
        .map_err(|e| e.to_string())?;
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

fn text(value: &str) -> SqlValue {
    SqlValue::Text(value.to_string())
}

fn opt_text(value: Option<&str>) -> SqlValue {
    value.map_or(SqlValue::Null, text)
}

fn int(value: impl Into<i64>) -> SqlValue {
    SqlValue::Integer(value.into())
}

fn opt_int<T: Into<i64>>(value: Option<T>) -> SqlValue {
    value.map_or(SqlValue::Null, int)
}

fn metadata(record: &ClaimRecord) -> Result<SqlValue, String> {
    serde_json::to_string(&record.metadata)
        .map(SqlValue::Text)
        .map_err(|e| e.to_string())
}

fn decode(row: &[SqlValue]) -> Result<ClaimRecord, String> {
    let record = parse(row)?;
    if let Some(SqlValue::Text(metadata)) = row.get(13) {
        serde_json::from_str::<serde_json::Map<String, Value>>(metadata)
            .map_err(|e| format!("claim metadata: {e}"))?;
    }
    claims::validate_record(&record)?;
    Ok(record)
}

/// A row as a record, unvalidated: the board shows a holder even when its
/// row would fail validation or its metadata is not JSON.
fn parse(row: &[SqlValue]) -> Result<ClaimRecord, String> {
    let at = |i: usize| row.get(i).unwrap_or(&SqlValue::Null);
    let need_text = |i: usize| {
        at(i)
            .text()
            .map(str::to_string)
            .ok_or_else(|| format!("claim column {i} is not text"))
    };
    let need_int = |i: usize| {
        at(i)
            .integer()
            .ok_or_else(|| format!("claim column {i} is not an integer"))
    };
    let maybe_text = |i: usize| match at(i) {
        SqlValue::Null => Ok(None),
        value => value
            .text()
            .map(|t| Some(t.to_string()))
            .ok_or_else(|| format!("claim column {i} is not text")),
    };
    let maybe_int = |i: usize| match at(i) {
        SqlValue::Null => Ok(None),
        value => value
            .integer()
            .map(Some)
            .ok_or_else(|| format!("claim column {i} is not an integer")),
    };
    let record = ClaimRecord {
        key: need_text(0)?,
        holder: need_text(1)?,
        schema_version: u32::try_from(need_int(2)?).map_err(|e| e.to_string())?,
        acquired_at: need_int(3)?,
        expires_at: maybe_int(4)?,
        pid: maybe_int(5)?
            .map(i32::try_from)
            .transpose()
            .map_err(|e| e.to_string())?,
        pid_unavailable: need_int(6)? != 0,
        host: need_text(7)?,
        machine_id: maybe_text(8)?,
        reason: maybe_text(9)?,
        harness: maybe_text(10)?,
        session_id: maybe_text(11)?,
        pid_provenance: maybe_text(12)?,
        metadata: serde_json::from_str(&need_text(13)?).unwrap_or_default(),
    };
    Ok(record)
}

fn record_for(db: &Db, key: &str) -> Result<Option<ClaimRecord>, String> {
    db.one(
        &format!("SELECT {COLUMNS} FROM claims WHERE key = ?1"),
        &[text(key)],
    )?
    .map(|row| decode(&row))
    .transpose()
}

/// A read must not mint a store: opening one creates `graph.db` and retires
/// the claims dir, so a status probe of a root that never held a claim would
/// write state there (a repo checkout, say). Only a store that is cleanly
/// missing counts: both paths report not-found and the nearest existing
/// ancestor is a writable dir, so an open would have created it. A path the
/// probe cannot stat, or one an open could never create, is unreadable state
/// and goes to the open, which names the fault.
fn store_absent(dir: &Path) -> bool {
    let Ok(db) = database_path_from_directory(dir) else {
        return false;
    };
    if !matches!(dir.try_exists(), Ok(false)) || !matches!(db.try_exists(), Ok(false)) {
        return false;
    }
    let Some(ancestor) = dir.ancestors().skip(1).find(|p| p.exists()) else {
        return false;
    };
    use std::os::unix::ffi::OsStrExt;
    let Ok(c_path) = std::ffi::CString::new(ancestor.as_os_str().as_bytes()) else {
        return false;
    };
    ancestor.is_dir() && unsafe { libc::access(c_path.as_ptr(), libc::W_OK | libc::X_OK) } == 0
}

pub(crate) fn read(key: &str, root: Option<&Path>) -> Result<Option<ClaimRecord>, String> {
    let dir = crate::claims_root::claims_dir(key, root)?;
    if !(shared(key) && primary_for(&dir)?.is_some()) && store_absent(&dir) {
        return Ok(None);
    }
    record_for(&db_in(&dir, key)?, key)
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
    record_for(&db_in(dir, &key)?, &key).map_err(|e| format!("Corrupted({e})"))
}

/// Every row under `dir`, unclassified. With a primary, the shared keys come
/// from it and the local file answers only for machine-local keys. One
/// unreadable row must not blind the whole scan; `claim status` on its key
/// still reports it corrupted.
fn rows_in(
    dir: &Path,
    prefix: Option<&str>,
    reader: fn(&[SqlValue]) -> Result<ClaimRecord, String>,
) -> Result<Vec<ClaimRecord>, String> {
    let may_share = prefix.is_none_or(|p| {
        SHARED_PREFIXES
            .iter()
            .any(|s| s.starts_with(p) || p.starts_with(s))
    });
    let primary = if may_share { primary_for(dir)? } else { None };
    let select = format!("SELECT {COLUMNS} FROM claims ORDER BY key");
    let mut records = Vec::new();
    if !store_absent(dir) {
        let local = Db::Local(open_directory(dir)?);
        records.extend(
            local
                .all(&select, &[])?
                .iter()
                .filter_map(|row| reader(row).ok())
                .filter(|r| primary.is_none() || !shared(&r.key)),
        );
    }
    if let Some(remote) = primary {
        records.extend(
            Db::Remote(remote)
                .all(&select, &[])?
                .iter()
                .filter_map(|row| reader(row).ok()),
        );
        records.sort_by(|a, b| a.key.cmp(&b.key));
    }
    records.retain(|record| prefix.is_none_or(|p| record.key.starts_with(p)));
    Ok(records)
}

/// Node claims whose lease has not run out: the holder projection the
/// backlog reads. An absent local store is an empty answer, never a creation.
pub(crate) fn unexpired_node_records() -> Result<Vec<ClaimRecord>, String> {
    let dir = directory(None)?;
    if primary_for(&dir)?.is_none() && !database_path_from_directory(&dir)?.exists() {
        return Ok(Vec::new());
    }
    let now = claims::now_ms();
    Ok(rows_in(&dir, Some("node:"), parse)?
        .into_iter()
        .filter(|r| r.expires_at.is_none_or(|at| at > now))
        .collect())
}

pub(crate) fn records_in(
    dir: &Path,
    prefix: Option<&str>,
    include_stale: bool,
) -> Result<Vec<ClaimRecord>, String> {
    let records = rows_in(dir, prefix, decode)?;
    if include_stale {
        return Ok(records);
    }
    // One primed witness for the whole scan: a per-row verdict paid one cold
    // `fno agents truth` interpreter per unresolved session, in series, and a
    // loaded host stretched a lead check-in past half an hour on 11 rows.
    let (witness, _answer) = crate::claim_verbs::session_witness_primed_for(&records);
    Ok(records
        .iter()
        .filter(|record| {
            let (state, _) = claims::classify_with_basis_and_exclusivity(
                record,
                None,
                &|pid| claims::probe_pid(pid),
                None,
                Some(&witness),
            );
            matches!(state, ClaimState::Live | ClaimState::Suspect)
        })
        .cloned()
        .collect())
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
    let db = open_for_key(key, options.root.as_deref())?;
    let observed = record_for(&db, key)?;
    // Takeover is a compare-and-swap on the row we classified. The clock
    // alone never decides: an expired lease whose pid or session is live stays held.
    let local_dead = observed.as_ref().filter(|r| {
        !matches!(
            claims::classify_with_session_witness(r, witness),
            ClaimState::Live | ClaimState::Suspect
        )
    });
    // Another machine's pid cannot be probed from here, so only its lease
    // decides, read on the store's clock: two hosts with skewed clocks still
    // agree on when a lease ran out.
    let foreign =
        local_dead.is_some_and(|r| !claims::is_same_machine(&r.host, r.machine_id.as_deref()));
    let record = claims::make_claim(key, holder, options);
    claims::validate_record(&record)?;
    let sql = format!("INSERT INTO claims ({COLUMNS}) VALUES (?1,?2,?3,{CLOCK},CASE WHEN ?4 IS NULL THEN NULL ELSE {CLOCK}+?4 END,?5,?6,?7,?8,?9,?10,?11,?12,?13)
        ON CONFLICT(key) DO UPDATE SET holder=excluded.holder, schema_version=excluded.schema_version,
        acquired_at=MAX(excluded.acquired_at,claims.acquired_at+1), expires_at=excluded.expires_at,
        pid=excluded.pid,pid_unavailable=excluded.pid_unavailable,host=excluded.host,machine_id=excluded.machine_id,
        reason=excluded.reason,harness=excluded.harness,session_id=excluded.session_id,pid_provenance=excluded.pid_provenance,metadata=excluded.metadata
        WHERE claims.holder=excluded.holder
        OR (claims.holder IS ?14 AND claims.acquired_at IS ?15 AND claims.expires_at IS ?16
            AND (?17 = 0 OR claims.expires_at <= {CLOCK}))
        RETURNING {COLUMNS}");
    let args = [
        text(key),
        text(holder),
        int(record.schema_version),
        opt_int(options.ttl_ms),
        opt_int(record.pid),
        int(record.pid_unavailable),
        text(&record.host),
        opt_text(record.machine_id.as_deref()),
        opt_text(record.reason.as_deref()),
        opt_text(record.harness.as_deref()),
        opt_text(record.session_id.as_deref()),
        opt_text(record.pid_provenance.as_deref()),
        metadata(&record)?,
        opt_text(local_dead.map(|r| r.holder.as_str())),
        opt_int(local_dead.map(|r| r.acquired_at)),
        opt_int(local_dead.and_then(|r| r.expires_at)),
        int(foreign),
    ];
    let result = db.one(&sql, &args)?.map(|row| decode(&row)).transpose()?;
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
            let existing = record_for(&db, key)?
                .ok_or_else(|| "claim changed after refused acquire; retry".to_string())?;
            Ok(AcquireOutcome::HeldByOther {
                holder: existing.holder,
                pid: existing.pid,
                host: existing.host,
            })
        }
    }
}

/// Every column of `record` in [`COLUMNS`] order.
fn columns_of(record: &ClaimRecord) -> Result<Vec<SqlValue>, String> {
    Ok(vec![
        text(&record.key),
        text(&record.holder),
        int(record.schema_version),
        int(record.acquired_at),
        opt_int(record.expires_at),
        opt_int(record.pid),
        int(record.pid_unavailable),
        text(&record.host),
        opt_text(record.machine_id.as_deref()),
        opt_text(record.reason.as_deref()),
        opt_text(record.harness.as_deref()),
        opt_text(record.session_id.as_deref()),
        opt_text(record.pid_provenance.as_deref()),
        metadata(record)?,
    ])
}

/// The predicate that matches exactly the row `record`, with its 14 values
/// bound from `?{first}` on.
fn exactly(first: usize) -> String {
    let names = COLUMNS.split(", ");
    names
        .enumerate()
        .map(|(i, name)| format!("{name} IS ?{}", first + i))
        .collect::<Vec<_>>()
        .join(" AND ")
}

pub(crate) fn replace_observed_at(
    path: &Path,
    observed: &ClaimRecord,
    next: &ClaimRecord,
) -> Result<Option<ClaimRecord>, String> {
    claims::validate_record(next)?;
    let dir = path
        .parent()
        .ok_or_else(|| "claim locator has no parent".to_string())?;
    let db = db_in(dir, &observed.key)?;
    let mut args = columns_of(next)?;
    args.extend(columns_of(observed)?);
    let set = COLUMNS
        .split(", ")
        .skip(1)
        .enumerate()
        .map(|(i, name)| format!("{name}=?{}", i + 2))
        .collect::<Vec<_>>()
        .join(",");
    db.one(
        &format!(
            "UPDATE claims SET {set} WHERE key=?1 AND {} RETURNING {COLUMNS}",
            exactly(15)
        ),
        &args,
    )?
    .map(|row| decode(&row))
    .transpose()
}

pub(crate) fn release(
    key: &str,
    holder: &str,
    root: Option<&Path>,
    events: Option<&Path>,
) -> Result<Option<ClaimRecord>, String> {
    let db = open_for_key(key, root)?;
    let removed = db
        .one(
            &format!("DELETE FROM claims WHERE key=?1 AND holder=?2 RETURNING {COLUMNS}"),
            &[text(key), text(holder)],
        )?
        .map(|row| decode(&row))
        .transpose()?;
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
    db_in(dir, &record.key)?
        .execute(
            &format!("DELETE FROM claims WHERE {}", exactly(1)),
            &columns_of(record)?,
        )
        .map(|n| n == 1)
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
    let db = open_for_key(key, root)?;
    let Some(observed) = record_for(&db, key)? else {
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
    let args = [
        text(key),
        text(holder),
        int(observed.acquired_at),
        int(ttl),
        opt_int(observed.expires_at),
        opt_int(next.pid),
        text(&next.host),
        opt_text(next.machine_id.as_deref()),
        opt_text(next.session_id.as_deref()),
        opt_int(observed.pid),
    ];
    Ok(db.execute(&sql, &args)? == 1)
}

/// Why `holder` no longer holds `key`, when another machine took it: the
/// notice a worker reads at its next stop. `None` when the holder still
/// holds it, the row is gone (a release), or the taker is this machine (a
/// handover between holders of one session).
pub(crate) fn lost_to_peer(key: &str, holder: &str) -> Option<String> {
    taker(read(key, None).ok()??, key, holder)
}

fn taker(current: ClaimRecord, key: &str, holder: &str) -> Option<String> {
    if current.holder == holder
        || claims::is_same_machine(&current.host, current.machine_id.as_deref())
    {
        return None;
    }
    Some(format!(
        "{key} is now held by {} on {} since {}: this machine's lease ran out on the store clock",
        current.holder,
        current.host,
        chrono::DateTime::from_timestamp_millis(current.acquired_at)
            .map(|t| t.to_rfc3339())
            .unwrap_or_else(|| current.acquired_at.to_string())
    ))
}

/// The shared claims the primary records for `machine`, leased ones only.
pub(crate) fn leased_on(remote: &Remote, machine: &str) -> Result<Vec<ClaimRecord>, String> {
    Ok(Db::Remote(remote.clone())
        .all(
            &format!(
                "SELECT {COLUMNS} FROM claims WHERE machine_id = ?1 AND expires_at IS NOT NULL"
            ),
            &[text(machine)],
        )?
        .iter()
        .filter_map(|row| decode(row).ok())
        .filter(|record| shared(&record.key))
        .collect())
}

/// Move every lease in `held` to at least `lease_ms` past the store clock,
/// in one statement. Each row must still carry the holder and acquired_at
/// it was read with, so a row a peer took is never extended. Returns the
/// keys it renewed.
pub(crate) fn extend_leases(
    remote: &Remote,
    machine: &str,
    held: &[ClaimRecord],
    lease_ms: i64,
) -> Result<Vec<String>, String> {
    if held.is_empty() {
        return Ok(Vec::new());
    }
    let mut args = vec![int(lease_ms), text(machine)];
    let mut rows = Vec::new();
    for record in held {
        let at = args.len();
        rows.push(format!("(?{},?{},?{})", at + 1, at + 2, at + 3));
        args.extend([
            text(&record.key),
            text(&record.holder),
            int(record.acquired_at),
        ]);
    }
    let sql = format!(
        "UPDATE claims SET expires_at = MAX(expires_at, {CLOCK} + ?1)
         WHERE machine_id = ?2 AND expires_at IS NOT NULL
           AND (key, holder, acquired_at) IN (VALUES {})
         RETURNING key",
        rows.join(",")
    );
    Ok(on_primary(remote, &sql, &args)?
        .rows
        .iter()
        .filter_map(|row| row.first().and_then(SqlValue::text).map(str::to_string))
        .collect())
}

/// [`lost_to_peer`] read on one primary.
pub(crate) fn lost_on(remote: &Remote, key: &str, holder: &str) -> Result<Option<String>, String> {
    Ok(record_for(&Db::Remote(remote.clone()), key)?.and_then(|c| taker(c, key, holder)))
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
    let db = open_for_key(key, root)?;
    // Raw columns, not a decoded record: the force path is the one way out
    // for a row that no longer validates.
    let removed: Option<(Option<String>, Option<i64>)> = db
        .one(
            "DELETE FROM claims WHERE key=?1 RETURNING holder, pid",
            &[text(key)],
        )?
        .map(|row| {
            (
                row.first().and_then(|v| v.text()).map(str::to_string),
                row.get(1).and_then(SqlValue::integer),
            )
        });
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
    let recheck = recheck.or(witness);
    reap_in_directory(
        &directory(root)?,
        apply,
        |_: &[ClaimRecord]| witness.map(boxed),
        |_: &[ClaimRecord]| recheck.map(boxed),
        None,
        None,
    )
}

/// A borrowed witness as the owned shape the reap phases hand around.
pub(crate) fn boxed<'a>(witness: SessionWitness<'a>) -> BoxedWitness<'a> {
    Box::new(move |record: &ClaimRecord| witness(record))
}

pub(crate) type BoxedWitness<'a> = Box<dyn Fn(&ClaimRecord) -> claims::SessionLiveness + 'a>;

/// Rows one reap chunk classifies together. Small, so one chunk's truth
/// batch can finish inside a reconcile beat's budget on a loaded machine.
const REAP_CHUNK: usize = 8;

/// Retire every same-machine claim that classifies Stale.
///
/// Rows go in chunks of [`REAP_CHUNK`]. Each chunk is classified with the
/// witness `scan_for` builds over that chunk, so a caller answers a chunk's
/// sessions in one batch. The apply step asks again, with the witness
/// `recheck_for` builds over the chunk's candidates or else the scan's own,
/// and deletes exactly the observed row, so a claim whose session came back
/// or that changed hands since the scan stays.
///
/// `deadline` bounds the pass, checked before every chunk and every delete.
/// A chunk starts only while the time left covers the slowest scan and the
/// slowest recheck seen so far: a recheck the deadline cuts
/// keeps every candidate, so the scan before it was spent for nothing. The
/// rows a pass never settled are `deferred`. A bounded pass
/// starts at a clock-chosen row and wraps, so live rows that cost a probe
/// cannot hold the same dead rows out of reach on every pass. One row that
/// fails to delete is named in `reap_failed`, and the pass goes on.
pub(crate) fn reap_in_directory<'w>(
    dir: &Path,
    apply: bool,
    mut scan_for: impl FnMut(&[ClaimRecord]) -> Option<BoxedWitness<'w>>,
    mut recheck_for: impl FnMut(&[ClaimRecord]) -> Option<BoxedWitness<'w>>,
    key: Option<&str>,
    deadline: Option<std::time::Instant>,
) -> Result<Value, String> {
    let spent = || deadline.is_some_and(|d| std::time::Instant::now() >= d);
    let mut records = records_in(dir, key, true)?
        .into_iter()
        .filter(|r| key.is_none_or(|k| r.key == k))
        .filter(|r| claims::is_same_machine(&r.host, r.machine_id.as_deref()))
        .collect::<Vec<_>>();
    if deadline.is_some() && !records.is_empty() {
        let start = (claims::now_ms().max(0) / 1000) as usize % records.len();
        records.rotate_left(start);
    }
    let (mut would_reap, mut reaped, mut settled) = (0, 0, 0);
    let mut failures = Vec::new();
    let slowest_scan = std::cell::Cell::new(std::time::Duration::ZERO);
    let slowest_recheck = std::cell::Cell::new(std::time::Duration::ZERO);
    let timed = |slowest: &std::cell::Cell<std::time::Duration>,
                 build: &mut dyn FnMut() -> Option<BoxedWitness<'w>>| {
        let started = std::time::Instant::now();
        let witness = build();
        slowest.set(slowest.get().max(started.elapsed()));
        witness
    };
    'pass: for chunk in records.chunks(REAP_CHUNK) {
        if deadline.is_some_and(|d| {
            d.saturating_duration_since(std::time::Instant::now())
                <= slowest_scan.get() + slowest_recheck.get()
        }) {
            break;
        }
        let witness = timed(&slowest_scan, &mut || scan_for(chunk));
        let mut candidates = Vec::new();
        for record in chunk {
            if claims::classify_with_session_witness(record, witness.as_deref())
                == ClaimState::Stale
            {
                candidates.push(record.clone());
            } else {
                settled += 1;
            }
        }
        would_reap += candidates.len();
        if !apply {
            settled += candidates.len();
            continue;
        }
        if candidates.is_empty() {
            continue;
        }
        let recheck = timed(&slowest_recheck, &mut || recheck_for(&candidates));
        let recheck = recheck.as_deref().or(witness.as_deref());
        for record in &candidates {
            if spent() {
                break 'pass;
            }
            settled += 1;
            if claims::classify_with_session_witness(record, recheck) != ClaimState::Stale {
                continue;
            }
            match delete_observed(dir, record) {
                Ok(true) => reaped += 1,
                Ok(false) => {}
                Err(e) => failures.push(format!("{}: {e}", record.key)),
            }
        }
    }
    let deferred = records.len() - settled;
    Ok(
        json!({"apply":apply,"scanned":records.len(),"would_reap":would_reap,"reaped":reaped,"deferred":deferred,"reap_failed":failures,"root":dir}),
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

    /// A shared key decides on the primary while a machine-local key stays
    /// in graph.db. A peer's lease is taken only past the store clock, a
    /// holder whose lease a peer took learns who took it, and a dead primary
    /// refuses with nothing written locally.
    #[test]
    fn store_conn_remote_shared_claims_decide_on_the_primary() {
        if claims::machine_id().is_empty() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let dir = directory(Some(root.path())).unwrap();
        let primary = crate::store_remote::test_primary::start();
        route_to_primary(Some((primary.remote.clone(), dir.clone())));
        let opts = |root: &Path| AcquireOpts {
            root: Some(root.to_path_buf()),
            pid: Some(std::process::id()),
            ttl_ms: Some(60_000),
            ..Default::default()
        };
        let held = |key: &str, holder: &str| {
            matches!(
                claims::acquire(key, holder, opts(root.path())),
                AcquireOutcome::Acquired(_)
            )
        };
        assert!(held("node:mine", "a") && held("build:cargo", "a"));
        assert!(!held("node:mine", "b"), "a live holder keeps its node");
        let keys_on_primary: Vec<String> = primary
            .db
            .lock()
            .unwrap()
            .prepare("SELECT key FROM claims")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(keys_on_primary, ["node:mine"]);
        let keys: Vec<String> = records_in(&dir, None, true)
            .unwrap()
            .into_iter()
            .map(|r| r.key)
            .collect();
        assert_eq!(keys, ["build:cargo", "node:mine"]);

        let now = claims::now_ms();
        let insert = format!(
            "INSERT INTO claims ({COLUMNS}) VALUES ({})",
            (1..=14)
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        for (key, expires) in [
            ("node:peer-live", now + 3_600_000),
            ("node:peer-gone", now - 1),
        ] {
            let mut peer = claims::make_claim(key, "peer", &opts(root.path()));
            (peer.host, peer.machine_id, peer.pid) =
                ("imac".into(), Some("imac-id".into()), Some(4242));
            (peer.acquired_at, peer.expires_at) = (now - 7_200_000, Some(expires));
            Db::Remote(primary.remote.clone())
                .execute(&insert, &columns_of(&peer).unwrap())
                .unwrap();
        }
        assert!(
            !held("node:peer-live", "b"),
            "a peer's unexpired lease holds"
        );
        assert!(
            held("node:peer-gone", "b"),
            "a lease past the store clock is taken"
        );

        primary
            .db
            .lock()
            .unwrap()
            .execute(
                "UPDATE claims SET holder = 'peer', host = 'imac', machine_id = 'imac-id', \
                 acquired_at = acquired_at + 1 WHERE key = 'node:mine'",
                [],
            )
            .unwrap();
        assert!(!claims::renew("node:mine", "a", 60_000, Some(root.path())).unwrap());
        let reason = lost_on(&primary.remote, "node:mine", "a").unwrap().unwrap();
        assert!(reason.contains("held by peer on imac"), "{reason}");

        let offline = tempfile::tempdir().unwrap();
        let offline_dir = directory(Some(offline.path())).unwrap();
        let dead = crate::store_remote::test_primary::dead();
        route_to_primary(Some((dead.clone(), offline_dir.clone())));
        let refused = claims::acquire("node:x", "a", opts(offline.path()));
        let AcquireOutcome::Error(error) = refused else {
            panic!("{refused:?}")
        };
        assert!(
            crate::store_remote::is_unreachable(&error)
                && error.contains(dead.url())
                && error.contains("store.remote_url"),
            "{error}"
        );
        assert!(!database_path_from_directory(&offline_dir).unwrap().exists());

        route_to_primary(None);
        assert!(matches!(
            claims::acquire("node:x", "a", opts(offline.path())),
            AcquireOutcome::Acquired(_)
        ));
        assert!(database_path_from_directory(&offline_dir).unwrap().exists());
    }

    /// A spent budget defers every row it did not reach and deletes nothing;
    /// the next unbounded pass reaps them. Dead holders here are cargo-shaped:
    /// a pid with no TTL, which frees the moment the pid is gone.
    #[test]
    fn reap_defers_rows_past_its_deadline_and_the_next_pass_reaps_them() {
        let root = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        for key in ["build:cargo", "test:cargo-run:0", "test:cargo-run:1"] {
            let outcome = claims::acquire(
                key,
                &format!("cargo:/tmp/w:{dead}"),
                AcquireOpts {
                    root: Some(root.path().to_path_buf()),
                    pid: Some(dead),
                    ..Default::default()
                },
            );
            assert!(
                matches!(outcome, AcquireOutcome::Acquired(_)),
                "{outcome:?}"
            );
        }
        let dir = directory(Some(root.path())).unwrap();
        let spent = Some(std::time::Instant::now());
        let held = reap_in_directory(&dir, true, |_| None, |_| None, None, spent).unwrap();
        assert_eq!(
            (held["reaped"].as_u64(), held["deferred"].as_u64()),
            (Some(0), Some(3))
        );
        assert_eq!(records_in(&dir, None, true).unwrap().len(), 3);
        let swept = reap_in_directory(&dir, true, |_| None, |_| None, None, None).unwrap();
        assert_eq!(
            (swept["reaped"].as_u64(), swept["deferred"].as_u64()),
            (Some(3), Some(0))
        );
        assert!(records_in(&dir, None, true).unwrap().is_empty());
    }
}
