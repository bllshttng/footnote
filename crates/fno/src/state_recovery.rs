//! Parked-store recovery. Sources are evidence and never modified or removed.
use rusqlite::{types::Value, Connection, OpenFlags, TransactionBehavior};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

const FAMILIES: &[&str] = &[
    "questions",
    "decisions",
    "events",
    "approvals",
    "graph-archive",
];
const EVENT_COLUMNS: &[&str] = &[
    "event_id",
    "row_hash",
    "ts_ms",
    "type",
    "source",
    "scope",
    "retention_class",
    "session_id",
    "node_id",
    "pr_number",
    "head_sha",
    "repo",
    "reject_reason",
    "line",
];

#[derive(Clone)]
struct MissingRow {
    family: String,
    table: String,
    columns: Vec<String>,
    values: Vec<Value>,
    identity: String,
    source: String,
}

pub(super) struct Sidecar {
    source: PathBuf,
    bytes: Vec<u8>,
}

fn read_sidecar(path: &Path) -> Result<Sidecar, String> {
    let source = std::fs::canonicalize(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let bytes = std::fs::read(&source).map_err(|e| e.to_string())?;
    Ok(Sidecar { source, bytes })
}

#[derive(Serialize)]
pub(super) struct Audit {
    pub root: PathBuf,
    mode: &'static str,
    state_mutations: usize,
    pub records: Vec<serde_json::Value>,
    pub missing_by_family: BTreeMap<String, usize>,
    pub missing_identities: Vec<serde_json::Value>,
    pub binary_sha256: String,
    pub errors: Vec<String>,
    pub packet_digest: String,
    pub zero_missing: bool,
    #[serde(skip)]
    missing: BTreeMap<String, MissingRow>,
}

fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn ro(path: &Path) -> Result<Connection, String> {
    let c = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| format!("{}: {e}", path.display()))?;
    c.busy_timeout(Duration::from_secs(5))
        .map_err(|e| e.to_string())?;
    Ok(c)
}

fn integrity(c: &Connection) -> Result<(), String> {
    let mut s = c
        .prepare("PRAGMA integrity_check")
        .map_err(|e| e.to_string())?;
    let rows = s
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    for row in rows {
        let row = row.map_err(|e| e.to_string())?;
        if row != "ok" {
            return Err(format!("integrity_check: {row}"));
        }
    }
    Ok(())
}

fn hash_file(path: &Path, hash: &mut Sha256) -> Result<(), String> {
    let mut file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut buf = [0u8; 65536];
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(())
}

fn source_hash(path: &Path) -> Result<String, String> {
    let mut h = Sha256::new();
    hash_file(path, &mut h)?;
    let wal = PathBuf::from(format!("{}-wal", path.display()));
    if std::fs::metadata(&wal).is_ok_and(|m| m.len() > 0) {
        h.update(b"WAL");
        hash_file(&wal, &mut h)?;
    }
    Ok(format!("{:x}", h.finalize()))
}

fn value_key(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Integer(n) => format!("i:{n}"),
        Value::Real(n) => format!("r:{:x}", n.to_bits()),
        Value::Text(s) => format!("t:{}:{s}", s.len()),
        Value::Blob(b) => format!(
            "b:{}",
            b.iter().map(|v| format!("{v:02x}")).collect::<String>()
        ),
    }
}

fn signature(values: &[Value]) -> String {
    let mut h = Sha256::new();
    for v in values {
        let k = value_key(v);
        h.update((k.len() as u64).to_be_bytes());
        h.update(k.as_bytes());
    }
    format!("{:x}", h.finalize())
}

fn tables(c: &Connection, family: &str) -> Result<Vec<(String, Vec<String>, Vec<usize>)>, String> {
    let mut s = c.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name").map_err(|e| e.to_string())?;
    let names = s
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    if matches!(family, "questions" | "decisions" | "events") {
        let version: i64 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if version != 2 || !names.iter().any(|n| n == "events") {
            return Err(format!("unsupported event schema v{version}"));
        }
        let mut s = c
            .prepare(
                "SELECT name FROM pragma_table_info('events') WHERE name <> 'seq' ORDER BY cid",
            )
            .map_err(|e| e.to_string())?;
        let cols = s
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        if cols != EVENT_COLUMNS {
            return Err("unsupported events columns".into());
        }
        return Ok(vec![("events".into(), cols, vec![0])]);
    }
    let mut out = Vec::new();
    for table in names {
        if table == "nodes_fts" || table.starts_with("nodes_fts_") {
            continue;
        }
        let mut s = c
            .prepare(&format!("PRAGMA table_info({})", quoted(&table)))
            .map_err(|e| e.to_string())?;
        let cols = s
            .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(5)?)))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        let mut pk: Vec<(i64, usize)> = cols
            .iter()
            .enumerate()
            .filter_map(|(i, (_, p))| (*p > 0).then_some((*p, i)))
            .collect();
        pk.sort();
        if pk.is_empty() {
            return Err(format!("owning table {table} has no primary key"));
        }
        out.push((
            table,
            cols.into_iter().map(|(n, _)| n).collect(),
            pk.into_iter().map(|(_, i)| i).collect(),
        ));
    }
    if out.is_empty() {
        return Err("no owning tables".into());
    }
    Ok(out)
}

pub(super) fn audit(root: &Path) -> Result<Audit, String> {
    let root = std::fs::canonicalize(root).map_err(|e| e.to_string())?;
    let backup = root.join("backups/state-root-migration");
    let mut stamps = std::fs::read_dir(&backup)
        .map_err(|e| format!("{}: {e}", backup.display()))?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    stamps.sort();
    let mut result = Audit {
        root: root.clone(),
        mode: "dry-run",
        state_mutations: 0,
        records: Vec::new(),
        missing_by_family: FAMILIES.iter().map(|s| (s.to_string(), 0)).collect(),
        missing_identities: Vec::new(),
        binary_sha256: source_hash(&std::env::current_exe().map_err(|e| e.to_string())?)?,
        errors: Vec::new(),
        packet_digest: String::new(),
        zero_missing: false,
        missing: BTreeMap::new(),
    };
    let mut verified_sources: BTreeMap<PathBuf, (String, u64, u64)> = BTreeMap::new();
    for family in FAMILIES {
        let live = root.join("db").join(format!("{family}.db"));
        let c = match ro(&live) {
            Ok(c) => c,
            Err(e) => {
                result.errors.push(e);
                continue;
            }
        };
        let specs = match tables(&c, family) {
            Ok(t) => t,
            Err(e) => {
                result.errors.push(format!("{}: {e}", live.display()));
                continue;
            }
        };
        integrity(&c).map_err(|e| format!("{}: {e}", live.display()))?;
        for (table, cols, pk) in specs {
            let mut hashes: BTreeMap<String, (String, String)> = BTreeMap::new();
            for stamp in &stamps {
                if !stamp.is_dir() {
                    result
                        .errors
                        .push(format!("unexpected backup entry {}", stamp.display()));
                    continue;
                }
                let source = stamp.join(format!("{family}.db"));
                if !source.exists() {
                    continue;
                }
                let inspected = (|| -> Result<serde_json::Value, String> {
                    if !verified_sources.contains_key(&source) {
                        let meta = std::fs::metadata(&source).map_err(|e| e.to_string())?;
                        verified_sources.insert(
                            source.clone(),
                            (source_hash(&source)?, meta.dev(), meta.ino()),
                        );
                    }
                    let src = ro(&source)?;
                    src.execute_batch("BEGIN").map_err(|e| e.to_string())?;
                    if !tables(&src, family)?
                        .iter()
                        .any(|s| s.0 == table && s.1 == cols && s.2 == pk)
                    {
                        return Err("owning schema differs from live".into());
                    }
                    if !result
                        .records
                        .iter()
                        .any(|r| r["source"] == serde_json::to_value(&source).unwrap())
                    {
                        integrity(&src)?;
                    }
                    let uri = format!(
                        "file:{}?mode=ro",
                        live.to_str()
                            .ok_or("invalid live path")?
                            .replace('%', "%25")
                            .replace('?', "%3F")
                            .replace('#', "%23")
                    );
                    src.execute("ATTACH DATABASE ?1 AS published", [uri])
                        .map_err(|e| e.to_string())?;
                    let table_sql = quoted(&table);
                    let join = pk
                        .iter()
                        .map(|i| format!("s.{} IS d.{}", quoted(&cols[*i]), quoted(&cols[*i])))
                        .collect::<Vec<_>>()
                        .join(" AND ");
                    if table == "events" {
                        let differs = cols
                            .iter()
                            .map(|name| format!("s.{} IS NOT d.{}", quoted(name), quoted(name)))
                            .collect::<Vec<_>>()
                            .join(" OR ");
                        let sql=format!("SELECT s.event_id FROM main.events s JOIN published.events d ON s.event_id=d.event_id WHERE {differs} LIMIT 1");
                        match src.query_row(&sql, [], |r| r.get::<_, String>(0)) {
                            Ok(id) => {
                                return Err(format!(
                                    "identity payload conflict {id} between {} and {}",
                                    source.display(),
                                    live.display()
                                ))
                            }
                            Err(rusqlite::Error::QueryReturnedNoRows) => {}
                            Err(e) => return Err(e.to_string()),
                        }
                        match src.query_row("SELECT s.event_id,d.event_id FROM main.events s JOIN published.events d ON s.row_hash=d.row_hash WHERE s.event_id<>d.event_id LIMIT 1",[],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))) {
                            Ok((id,other))=>return Err(format!("row_hash conflict {id} with {other}, sources {} and {}",source.display(),live.display())),
                            Err(rusqlite::Error::QueryReturnedNoRows)=>{},Err(e)=>return Err(e.to_string()),
                        }
                    }
                    let count: i64 = src
                        .query_row(&format!("SELECT count(*) FROM main.{table_sql}"), [], |r| {
                            r.get(0)
                        })
                        .map_err(|e| e.to_string())?;
                    let select = cols
                        .iter()
                        .map(|name| format!("s.{}", quoted(name)))
                        .collect::<Vec<_>>()
                        .join(",");
                    let sql=format!("SELECT {select} FROM main.{table_sql} s WHERE NOT EXISTS(SELECT 1 FROM published.{table_sql} d WHERE {join})");
                    let mut stmt = src.prepare(&sql).map_err(|e| e.to_string())?;
                    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
                    let mut missing = 0usize;
                    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
                        let values = (0..cols.len())
                            .map(|i| row.get(i))
                            .collect::<Result<Vec<Value>, _>>()
                            .map_err(|e| e.to_string())?;
                        let identity =
                            signature(&pk.iter().map(|i| values[*i].clone()).collect::<Vec<_>>());
                        let key = format!("{family}/{table}/{identity}");
                        let payload = signature(&values);
                        if table == "events" {
                            let hash = value_key(&values[1]);
                            if let Some((other, locator)) = hashes.get(&hash) {
                                if other != &identity {
                                    return Err(format!("row_hash conflict {key} with {other}, sources {locator} and {}",source.display()));
                                }
                            }
                            hashes.insert(hash, (identity.clone(), source.display().to_string()));
                        }
                        missing += 1;
                        if let Some(earlier) = result.missing.get(&key) {
                            if signature(&earlier.values) != payload {
                                return Err(format!(
                                    "backup identity conflict {key}: {} and {}",
                                    earlier.source,
                                    source.display()
                                ));
                            }
                        } else {
                            result.missing.insert(
                                key,
                                MissingRow {
                                    family: family.to_string(),
                                    table: table.clone(),
                                    columns: cols.clone(),
                                    values,
                                    identity,
                                    source: source.display().to_string(),
                                },
                            );
                        }
                    }
                    let (digest, dev, ino) = verified_sources.get(&source).unwrap();
                    Ok(
                        serde_json::json!({"stamp":stamp.file_name(),"family":family,"table":table,"source":source,"live":live,"source_sha256":digest,"dev":dev,"ino":ino,"rows":count,"missing":missing,"status":"audited"}),
                    )
                })();
                match inspected {
                    Ok(record) => result.records.push(record),
                    Err(e) => result
                        .errors
                        .push(format!("{} {table}: {e}", source.display())),
                }
            }
        }
    }
    for (source, (digest, dev, ino)) in verified_sources {
        match (source_hash(&source), std::fs::metadata(&source)) {
            (Ok(now), Ok(meta)) if now == digest && meta.dev() == dev && meta.ino() == ino => {}
            _ => result
                .errors
                .push(format!("{}: source changed during audit", source.display())),
        }
    }
    for row in result.missing.values() {
        *result.missing_by_family.get_mut(&row.family).unwrap() += 1;
    }
    result.missing_identities=result.missing.iter().map(|(key,row)|serde_json::json!({"key":key,"family":row.family,"table":row.table,"event_id":if row.table=="events" {match &row.values[0] {Value::Text(id)=>Some(id.as_str()),_=>None}} else {None},"identity_hash":row.identity,"payload_hash":signature(&row.values),"source":row.source})).collect();
    let identities: Vec<_> = result
        .missing
        .iter()
        .map(|(key, row)| (key, signature(&row.values)))
        .collect();
    let packet=serde_json::to_vec(&serde_json::json!({"root":result.root,"records":result.records,"identities":identities,"errors":result.errors,"binary_sha256":result.binary_sha256,"replay_policy":"history-only-v1"})).map_err(|e|e.to_string())?;
    result.packet_digest = format!("{:x}", Sha256::digest(packet));
    result.zero_missing = result.missing.is_empty() && result.errors.is_empty();
    Ok(result)
}

struct Lock {
    file: std::fs::File,
}
impl Lock {
    fn take(root: &Path) -> Result<Self, String> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("db/.recovery.lock"))
            .map_err(|e| e.to_string())?;
        if unsafe {
            libc::flock(
                std::os::fd::AsRawFd::as_raw_fd(&file),
                libc::LOCK_EX | libc::LOCK_NB,
            )
        } != 0
        {
            return Err("recovery writer lock is held".into());
        }
        Ok(Self { file })
    }
}
impl Drop for Lock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(std::os::fd::AsRawFd::as_raw_fd(&self.file), libc::LOCK_UN);
        }
    }
}

pub(super) fn apply(
    root: &Path,
    expected: &str,
    sidecars: &[Sidecar],
) -> Result<serde_json::Value, String> {
    let canonical_root = std::fs::canonicalize(root).map_err(|e| e.to_string())?;
    let root = canonical_root.as_path();
    if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("packet changed: digest must be 64 hexadecimal characters".into());
    }
    let _lock = Lock::take(root)?;
    let batch = audit(root)?;
    if !batch.errors.is_empty() {
        return Err(format!("audit refused: {:?}", batch.errors));
    }
    if ["approvals", "graph-archive"]
        .iter()
        .any(|f| batch.missing_by_family[*f] > 0)
    {
        return Err("non-event owning rows need replay-safe recovery before apply".into());
    }
    let dir = root.join("backups/state-recovery").join(expected);
    let manifest = dir.join("batch.json");
    let saved: serde_json::Value = if manifest.exists() {
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
        let original = saved["records"]
            .as_array()
            .ok_or("invalid saved batch records")?;
        if saved["packet_digest"] != expected
            || saved["root"] != serde_json::to_value(&batch.root).map_err(|e| e.to_string())?
            || saved["binary_sha256"] != batch.binary_sha256
            || original.len() != batch.records.len()
        {
            return Err("saved packet does not match current root, executable or sources".into());
        }
        for (old, new) in original.iter().zip(&batch.records) {
            for key in ["source", "source_sha256", "dev", "ino", "family", "table"] {
                if old[key] != new[key] {
                    return Err(format!("saved packet source changed: {}", new["source"]));
                }
            }
        }
        let approved = saved["missing_identities"]
            .as_array()
            .ok_or("invalid approved identities")?;
        for identity in &batch.missing_identities {
            if !approved.iter().any(|old| {
                old["key"] == identity["key"] && old["payload_hash"] == identity["payload_hash"]
            }) {
                return Err("unapproved missing identity. Obtain a new packet".into());
            }
        }
        saved
    } else {
        if batch.packet_digest != expected {
            return Err("packet changed. Re-audit and obtain renewed approval".into());
        }
        serde_json::to_value(&batch).map_err(|e| e.to_string())?
    };
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    if !manifest.exists() {
        std::fs::write(
            &manifest,
            serde_json::to_vec_pretty(&saved).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        std::fs::File::open(&manifest)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
    }
    let mut snapshots = Vec::new();
    let prior_snapshots: Vec<serde_json::Value> = if dir.join("snapshots.json").exists() {
        serde_json::from_slice(
            &std::fs::read(dir.join("snapshots.json")).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?
    } else {
        Vec::new()
    };
    for family in FAMILIES {
        let path = root.join("db").join(format!("{family}.db"));
        let src = ro(&path)?;
        let target = dir.join(format!("{family}.db"));
        if target.exists() {
            let old = prior_snapshots
                .iter()
                .find(|v| v["family"] == *family)
                .ok_or("snapshot exists without a complete verified manifest")?;
            if old["sha256"] != source_hash(&target)? {
                return Err("snapshot hash changed".into());
            }
            integrity(&ro(&target)?)?;
            snapshots.push(old.clone());
            continue;
        }
        let mut dst = Connection::open(&target).map_err(|e| e.to_string())?;
        let backup = rusqlite::backup::Backup::new(&src, &mut dst).map_err(|e| e.to_string())?;
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            match backup.step(256).map_err(|e| e.to_string())? {
                rusqlite::backup::StepResult::Done => break,
                rusqlite::backup::StepResult::More => {}
                _ if std::time::Instant::now() >= deadline => {
                    return Err("snapshot backup exceeded 30 seconds".into())
                }
                _ => std::thread::sleep(Duration::from_millis(5)),
            }
            if std::time::Instant::now() >= deadline {
                return Err("snapshot backup exceeded 30 seconds".into());
            }
        }
        drop(backup);
        integrity(&dst)?;
        let schema_version: i64 = dst
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        let mut counts = BTreeMap::new();
        let mut statement = dst
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            )
            .map_err(|e| e.to_string())?;
        let names = statement
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        drop(statement);
        for table in names {
            let count: i64 = dst
                .query_row(
                    &format!("SELECT count(*) FROM {}", quoted(&table)),
                    [],
                    |r| r.get(0),
                )
                .map_err(|e| e.to_string())?;
            let seq: bool = dst
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name='seq')",
                    [&table],
                    |r| r.get(0),
                )
                .map_err(|e| e.to_string())?;
            let high_water: Option<i64> = if seq {
                dst.query_row(
                    &format!("SELECT max(seq) FROM {}", quoted(&table)),
                    [],
                    |r| r.get(0),
                )
                .map_err(|e| e.to_string())?
            } else {
                None
            };
            counts.insert(
                table,
                serde_json::json!({"rows":count,"seq_high_water":high_water}),
            );
        }
        drop(dst);
        let m = std::fs::metadata(&path).map_err(|e| e.to_string())?;
        snapshots.push(serde_json::json!({"family":family,"source":std::fs::canonicalize(&path).map_err(|e|e.to_string())?,"dev":m.dev(),"ino":m.ino(),"snapshot":target,"sha256":source_hash(&target)?,"integrity":"ok","created_at":chrono::Utc::now().to_rfc3339(),"schema_version":schema_version,"tables":counts}));
    }
    for (i, sidecar) in sidecars.iter().enumerate() {
        let path = &sidecar.source;
        let bytes = &sidecar.bytes;
        let target = dir.join(format!("sidecar-{i}"));
        if target.exists() {
            let old = prior_snapshots
                .iter()
                .find(|v| v["source"] == serde_json::to_value(path).unwrap())
                .ok_or("required sidecar was not snapshotted")?;
            if old["sha256"] != source_hash(&target)? {
                return Err("sidecar snapshot hash changed".into());
            }
            snapshots.push(old.clone());
            continue;
        }
        std::fs::write(&target, bytes).map_err(|e| e.to_string())?;
        snapshots.push(serde_json::json!({"source":path,"snapshot":target,"sha256":format!("{:x}",Sha256::digest(bytes))}));
    }
    std::fs::write(
        dir.join("snapshots.json"),
        serde_json::to_vec_pretty(&snapshots).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    std::fs::File::open(dir.join("snapshots.json"))
        .and_then(|file| file.sync_all())
        .map_err(|e| e.to_string())?;
    std::fs::File::open(&dir)
        .and_then(|file| file.sync_all())
        .map_err(|e| e.to_string())?;
    let mut inserted = BTreeMap::new();
    for family in &FAMILIES[..3] {
        for record in &batch.records {
            if record["family"] == *family
                && record["source_sha256"]
                    != source_hash(Path::new(
                        record["source"].as_str().ok_or("invalid source path")?,
                    ))?
            {
                return Err("source changed after audit".into());
            }
        }
        let path = root.join("db").join(format!("{family}.db"));
        let mut c = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_WRITE)
            .map_err(|e| e.to_string())?;
        c.busy_timeout(Duration::from_secs(5))
            .map_err(|e| e.to_string())?;
        let tx = c
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS recovery_history(event_id TEXT PRIMARY KEY, batch TEXT NOT NULL, source TEXT NOT NULL, applied_at TEXT NOT NULL)").map_err(|e|e.to_string())?;
        let mut n = 0usize;
        for row in batch.missing.values().filter(|r| r.family == *family) {
            let event_id = match &row.values[0] {
                Value::Text(id) => id,
                _ => return Err("invalid event identity".into()),
            };
            let sql = format!(
                "SELECT {} FROM events WHERE event_id=?1",
                row.columns
                    .iter()
                    .map(|n| quoted(n))
                    .collect::<Vec<_>>()
                    .join(",")
            );
            let existing = tx.query_row(&sql, [event_id], |r| {
                (0..row.columns.len())
                    .map(|i| r.get(i))
                    .collect::<Result<Vec<Value>, _>>()
            });
            match existing {
                Ok(values) if values == row.values => continue,
                Ok(_) => return Err(format!("concurrent identity conflict {event_id}")),
                Err(rusqlite::Error::QueryReturnedNoRows) => {}
                Err(e) => return Err(e.to_string()),
            }
            let sql = format!(
                "INSERT INTO events ({}) VALUES ({})",
                row.columns
                    .iter()
                    .map(|n| quoted(n))
                    .collect::<Vec<_>>()
                    .join(","),
                vec!["?"; row.columns.len()].join(",")
            );
            tx.execute(&sql, rusqlite::params_from_iter(row.values.iter()))
                .map_err(|e| format!("{}: {e}", row.source))?;
            tx.execute("INSERT INTO recovery_history(event_id,batch,source,applied_at) VALUES (?1,?2,?3,?4)",rusqlite::params![event_id,expected,row.source,chrono::Utc::now().to_rfc3339()]).map_err(|e|e.to_string())?;
            n += 1;
        }
        tx.commit().map_err(|e| e.to_string())?;
        inserted.insert(family.to_string(), n);
    }
    let after = audit(root)?;
    Ok(
        serde_json::json!({"mode":"apply","packet_digest":expected,"inserted":inserted,"snapshot_manifest":dir.join("snapshots.json"),"remaining":after}),
    )
}

pub(super) fn run(args: &[OsString]) -> i32 {
    let mut root = None;
    let mut do_apply = false;
    let mut digest = None;
    let mut copy = false;
    let mut sidecars = Vec::new();
    let mut approval = None;
    let mut packet = None;
    let mut it = args.iter();
    while let Some(tok) = it.next() {
        match tok.to_str().unwrap_or("") {
            "--root" => root = it.next().map(PathBuf::from),
            "--dry-run" => {}
            "--apply" => do_apply = true,
            "--packet-digest" => digest = it.next().and_then(|s| s.to_str().map(str::to_owned)),
            "--copy-proof" => copy = true,
            "--approval" => approval = it.next().and_then(|s| s.to_str().map(str::to_owned)),
            "--packet" => packet = it.next().map(PathBuf::from),
            "--sidecar" => match it.next() {
                Some(p) => sidecars.push(PathBuf::from(p)),
                None => {
                    eprintln!("--sidecar requires a path");
                    return 2;
                }
            },
            "--help" => {
                println!("fno doctor event recover --root PATH [--dry-run]\nCopy proof: --apply --copy-proof --packet-digest SHA256 [--sidecar PATH]...\nApproved apply: --apply --packet-digest SHA256 --packet FILE --approval d-ID\nApproval must be a live operator law naming the root, audit digest and packet file hash. Packet must carry verified consumer proof and sidecars. Backups are never pruned.");
                return 0;
            }
            other => {
                eprintln!("unknown recovery argument {other}");
                return 2;
            }
        }
    }
    let root = root.unwrap_or_else(|| {
        std::env::var_os("FNO_AGENTS_HOME")
            .map(PathBuf::from)
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".fno")
            })
    });
    let result = if do_apply {
        let live = crate::live_store_fence::operator_state_root();
        let is_live = live
            .as_ref()
            .is_some_and(|p| std::fs::canonicalize(p).ok() == std::fs::canonicalize(&root).ok());
        match digest {
            None => Err("--packet-digest is required for apply".into()),
            Some(expected) if copy && !is_live => copy_root(&root,live.as_deref()).and_then(|()|sidecars.iter().map(|path|read_sidecar(path)).collect::<Result<Vec<_>,_>>()).and_then(|sidecars|apply(&root,&expected,&sidecars)),
            Some(expected) => match (approval,packet) {
                (Some(id),Some(path)) => approved_sidecars(&root,&expected,&id,&path).and_then(|sidecars|apply(&root,&expected,&sidecars)),
                _ => Err("live apply is held. Obtain an exact user-approved packet and verified deployed consumer coverage".into()),
            }
        }
    } else {
        audit(&root).and_then(|a| serde_json::to_value(a).map_err(|e| e.to_string()))
    };
    match result {
        Ok(value) => {
            let verdict = value.get("remaining").unwrap_or(&value);
            let refused = verdict
                .get("errors")
                .and_then(|e| e.as_array())
                .is_some_and(|e| !e.is_empty())
                || (do_apply && verdict["zero_missing"] != true);
            println!("{}", serde_json::to_string_pretty(&value).unwrap());
            if refused {
                1
            } else {
                0
            }
        }
        Err(e) => {
            eprintln!("recovery refused: {e}");
            1
        }
    }
}

fn copy_root(root: &Path, live: Option<&Path>) -> Result<(), String> {
    let root = std::fs::canonicalize(root).map_err(|e| e.to_string())?;
    let temp = std::fs::canonicalize(std::env::temp_dir()).map_err(|e| e.to_string())?;
    let private_tmp = std::fs::canonicalize("/tmp").map_err(|e| e.to_string())?;
    if !root.starts_with(&temp) && !root.starts_with(private_tmp) {
        return Err(
            "--copy-proof requires a physical root under the OS temporary directory".into(),
        );
    }
    let live = live.ok_or("cannot identify operator state root for copy proof")?;
    for family in FAMILIES {
        let copied = std::fs::metadata(root.join("db").join(format!("{family}.db")))
            .map_err(|e| e.to_string())?;
        if let Ok(original) = std::fs::metadata(live.join("db").join(format!("{family}.db"))) {
            if copied.dev() == original.dev() && copied.ino() == original.ino() {
                return Err(format!("{family} is a live inode, not a copy"));
            }
        }
    }
    Ok(())
}

fn approved_sidecars(
    root: &Path,
    digest: &str,
    id: &str,
    path: &Path,
) -> Result<Vec<Sidecar>, String> {
    let root = std::fs::canonicalize(root).map_err(|e| e.to_string())?;
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let proof: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    let packet_hash = format!("{:x}", Sha256::digest(&bytes));
    if proof["root"] != serde_json::to_value(&root).map_err(|e| e.to_string())?
        || proof["audit_digest"] != digest
        || proof["historical_effects"] != 0
        || proof["controls_once"] != true
    {
        return Err("packet root, audit or replay proof is invalid".into());
    }
    let inventory = proof["consumer_inventory"]
        .as_array()
        .ok_or("missing consumer inventory")?;
    for name in [
        "subscribe",
        "status_fanout",
        "lead_answer_wake",
        "attention_reply",
        "question_lifecycle",
        "question_sweep",
        "other_action_readers",
    ] {
        if !inventory.iter().any(|v| v.as_str() == Some(name)) {
            return Err(format!("consumer proof missing: {name}"));
        }
    }
    let artifacts = proof["artifacts"]
        .as_array()
        .filter(|a| !a.is_empty())
        .ok_or("missing verified proof artifacts")?;
    for artifact in artifacts {
        let path = Path::new(artifact["path"].as_str().ok_or("artifact path missing")?);
        if artifact["sha256"] != source_hash(path)? {
            return Err(format!("proof artifact changed: {}", path.display()));
        }
    }
    let mut sidecars = Vec::new();
    for sidecar in proof["sidecars"]
        .as_array()
        .ok_or("missing sidecar inventory")?
    {
        let path = PathBuf::from(sidecar["path"].as_str().ok_or("sidecar path missing")?);
        let captured = read_sidecar(&path)?;
        if sidecar["sha256"] != format!("{:x}", Sha256::digest(&captured.bytes)) {
            return Err(format!("consumer sidecar changed: {}", path.display()));
        }
        sidecars.push(captured);
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let result = std::process::Command::new(exe)
        .args(["inbox", "decisions", id, "--json"])
        .output()
        .map_err(|e| e.to_string())?;
    if !result.status.success() {
        return Err("operator approval read failed".into());
    }
    let decisions: serde_json::Value =
        serde_json::from_slice(&result.stdout).map_err(|e| format!("approval unreadable: {e}"))?;
    if decisions["damaged"] != 0 || decisions["truncated"] != false {
        return Err("operator approval index is incomplete".into());
    }
    let expected = format!(
        "approve state recovery {} {digest} {packet_hash}",
        root.display()
    );
    let authorized = decisions["decisions"].as_array().is_some_and(|rows| {
        rows.iter().any(|row| {
            row["decision_id"] == id
                && row["lifecycle"] == "live"
                && row["lane"] == "law"
                && matches!(
                    row["authority_source"].as_str(),
                    Some("operator" | "chat_attested")
                )
                && row["decision"] == expected
        })
    });
    if !authorized {
        return Err(format!("operator approval must state exactly: {expected}"));
    }
    for family in FAMILIES {
        crate::live_store_fence::refuse_worktree_build_on_operator_store(
            &root.join("db").join(format!("{family}.db")),
        )?;
    }
    Ok(sidecars)
}
