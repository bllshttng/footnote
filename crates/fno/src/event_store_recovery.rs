//! Lossless table recovery from an offline event-store image, including its WAL.
use rusqlite::{types::Value, Connection, OpenFlags};
use serde_json::{json, Value as Json};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn stamp(path: &Path) -> Result<Option<(u64, u64, u64, i64, i64)>, String> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    if !meta.is_file() || meta.nlink() != 1 {
        return Err(format!(
            "{}: requires a regular offline copy, not a link",
            path.display()
        ));
    }
    Ok(Some((
        meta.dev(),
        meta.ino(),
        meta.len(),
        meta.mtime(),
        meta.mtime_nsec(),
    )))
}

fn digest(conn: &Connection, name: &str) -> Result<(u64, String), String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT * FROM {} NOT INDEXED ORDER BY rowid",
            quoted(name)
        ))
        .map_err(|e| e.to_string())?;
    let columns = stmt.column_count();
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    let mut hash = Sha256::new();
    let mut count = 0;
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        for column in 0..columns {
            let value: Value = row.get(column).map_err(|e| e.to_string())?;
            let bytes = format!("{value:?}");
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }
        count += 1;
    }
    Ok((count, format!("{:x}", hash.finalize())))
}

fn checked(conn: &Connection, pragma: &str) -> Result<(), String> {
    let mut stmt = conn.prepare(pragma).map_err(|e| e.to_string())?;
    let results = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    for result in results {
        let result = result.map_err(|e| e.to_string())?;
        if result != "ok" {
            return Err(format!("{pragma}: {result}"));
        }
    }
    Ok(())
}

fn recover(source: &Path, output: &Path) -> Result<Json, String> {
    if rusqlite::version_number() < 3_051_003 {
        return Err("recovery requires SQLite 3.51.3 or later".into());
    }
    stamp(source)?;
    let source = source.canonicalize().map_err(|e| e.to_string())?;
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()
        .map_err(|e| e.to_string())?;
    if crate::live_store_fence::operator_state_root()
        .and_then(|p| p.canonicalize().ok())
        .is_some_and(|p| source.starts_with(&p) || parent.starts_with(&p))
    {
        return Err(
            "source and output must be offline copies outside the operator state root".into(),
        );
    }
    std::fs::create_dir(output).map_err(|e| format!("{}: {e}", output.display()))?;
    let scratch = tempfile::tempdir_in(output).map_err(|e| e.to_string())?;
    let image = scratch.path().join("source.db");
    let mut wal = source.as_os_str().to_os_string();
    wal.push("-wal");
    let wal = PathBuf::from(wal);
    let before = (stamp(&source)?, stamp(&wal)?);
    if before.0.is_none() {
        return Err("source disappeared".into());
    }
    std::fs::copy(&source, &image).map_err(|e| e.to_string())?;
    let wal_bytes = match before.1 {
        Some(meta) => {
            std::fs::copy(&wal, image.with_file_name("source.db-wal"))
                .map_err(|e| e.to_string())?;
            meta.2
        }
        None => 0,
    };
    if before != (stamp(&source)?, stamp(&wal)?) {
        return Err("source or WAL changed during capture; supply a quiescent offline copy".into());
    }
    let original = Connection::open_with_flags(&image, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| e.to_string())?;
    original.execute_batch("BEGIN").map_err(|e| e.to_string())?;
    let version: i64 = original
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|e| e.to_string())?;
    if version != crate::event_store::SCHEMA_VERSION {
        return Err(format!("unsupported event schema {version}"));
    }
    let schema: Vec<(String, String, String)> = original.prepare(
        "SELECT type,name,sql FROM sqlite_schema WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' ORDER BY rowid"
    ).map_err(|e| e.to_string())?.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .map_err(|e| e.to_string())?.collect::<Result<_, _>>().map_err(|e| e.to_string())?;
    for required in [
        "events",
        "events_meta",
        "ingest_cursor",
        "event_observation_state",
        "event_observation_pending",
    ] {
        if !schema
            .iter()
            .any(|(kind, name, _)| kind == "table" && name == required)
        {
            return Err(format!("missing event-store table {required}"));
        }
    }
    let candidate = scratch.path().join("events.db");
    let mut restored = Connection::open(&candidate).map_err(|e| e.to_string())?;
    let tx = restored.transaction().map_err(|e| e.to_string())?;
    let mut tables = serde_json::Map::new();
    for (_, name, sql) in schema.iter().filter(|(kind, _, _)| kind == "table") {
        tx.execute_batch(sql).map_err(|e| format!("{name}: {e}"))?;
        if name == "ingest_cursor" {
            continue;
        }
        let mut select = original
            .prepare(&format!(
                "SELECT * FROM {} NOT INDEXED ORDER BY rowid",
                quoted(name)
            ))
            .map_err(|e| e.to_string())?;
        let columns = select.column_count();
        let mut insert = tx
            .prepare(&format!(
                "INSERT INTO {} VALUES ({})",
                quoted(name),
                vec!["?"; columns].join(",")
            ))
            .map_err(|e| e.to_string())?;
        let mut rows = select.query([]).map_err(|e| e.to_string())?;
        while let Some(row) = rows.next().map_err(|e| format!("{name}: {e}"))? {
            let values = (0..columns)
                .map(|column| row.get::<_, Value>(column))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?;
            insert
                .execute(rusqlite::params_from_iter(values))
                .map_err(|e| e.to_string())?;
        }
        let before = digest(&original, name)?;
        let after = digest(&tx, name)?;
        if before != after {
            return Err(format!("{name}: recovery digest mismatch"));
        }
        tables.insert(
            name.clone(),
            json!({"rows": before.0, "sha256": before.1, "equal": true}),
        );
    }
    for (_, _, sql) in schema.iter().filter(|(kind, _, _)| kind != "table") {
        tx.execute_batch(sql).map_err(|e| e.to_string())?;
    }
    tx.pragma_update(None, "user_version", version)
        .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    checked(&restored, "PRAGMA quick_check")?;
    checked(&restored, "PRAGMA integrity_check")?;
    drop(restored);
    let destination = output.join("events.db");
    std::fs::hard_link(&candidate, &destination).map_err(|e| e.to_string())?;
    let result = json!({"source": source, "output": destination, "wal_bytes": wal_bytes,
        "sqlite": rusqlite::version(), "tables": tables, "ingest_cursor_rows": 0,
        "quick_check": "ok", "integrity_check": "ok", "live_installation": false});
    std::fs::write(
        output.join("recovery.json"),
        serde_json::to_vec_pretty(&result).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    Ok(result)
}

pub(super) fn run(args: &[OsString]) -> i32 {
    let mut source = None;
    let mut output = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.to_str().unwrap_or("") {
            "--source" => source = args.next().map(PathBuf::from),
            "--output" => output = args.next().map(PathBuf::from),
            "--help" => {
                println!("fno doctor event recover --copy-store --source OFFLINE_DB --output NEW_DIRECTORY\nCopies DB and WAL into scratch, preserves all readable SQL tables and resets derived ingest cursors. Refuses live sources and existing output directories. Never installs the candidate.");
                return 0;
            }
            _ => {
                eprintln!("unknown copy recovery argument {}", arg.to_string_lossy());
                return 2;
            }
        }
    }
    let result = match (source, output) {
        (Some(source), Some(output)) => recover(&source, &output),
        _ => {
            eprintln!("--source and --output are required");
            return 2;
        }
    };
    match result {
        Ok(result) => {
            println!("{result}");
            0
        }
        Err(error) => {
            eprintln!("copy recovery refused: {error}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_recovery_keeps_wal_rows_and_refuses_clobber() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.db");
        let conn = Connection::open(&source).unwrap();
        conn.execute_batch("PRAGMA user_version=2; CREATE TABLE events(seq INTEGER PRIMARY KEY, line TEXT); CREATE TABLE events_meta(k TEXT PRIMARY KEY,v TEXT); CREATE TABLE ingest_cursor(k TEXT PRIMARY KEY,v INTEGER); CREATE TABLE event_observation_state(k TEXT PRIMARY KEY,v TEXT); CREATE TABLE event_observation_pending(k TEXT PRIMARY KEY,v TEXT); INSERT INTO events VALUES(1,'main'); INSERT INTO events_meta VALUES('epoch','kept'); INSERT INTO event_observation_state VALUES('s','kept'); INSERT INTO event_observation_pending VALUES('p','kept');").unwrap();
        let root: u64 = conn
            .query_row(
                "SELECT rootpage FROM sqlite_schema WHERE name='ingest_cursor'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let size: u64 = conn
            .query_row("PRAGMA page_size", [], |row| row.get(0))
            .unwrap();
        drop(conn);
        use std::io::{Seek, SeekFrom, Write};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&source)
            .unwrap();
        file.seek(SeekFrom::Start((root - 1) * size)).unwrap();
        file.write_all(&vec![0; size as usize]).unwrap();
        drop(file);
        let writer = Connection::open(&source).unwrap();
        writer.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; INSERT INTO events VALUES(2,'wal');").unwrap();
        let before = std::fs::read(&source).unwrap();
        let output = dir.path().join("recovered");
        let result = recover(&source, &output).unwrap();
        assert_eq!(result["tables"]["events"]["rows"], 2);
        assert!(result["wal_bytes"].as_u64().unwrap() > 0);
        assert_eq!(result["tables"]["event_observation_pending"]["rows"], 1);
        assert_eq!(std::fs::read(&source).unwrap(), before);
        assert!(recover(&source, &output).is_err());
        let conn = Connection::open(output.join("events.db")).unwrap();
        checked(&conn, "PRAGMA integrity_check").unwrap();
        assert_eq!(
            conn.query_row("SELECT line FROM events WHERE seq=2", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "wal"
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM ingest_cursor", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}
