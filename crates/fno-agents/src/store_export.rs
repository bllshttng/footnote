//! `fno-agents store-export --out <file> [--url <primary>]`: copy every table
//! on the shared primary into a local SQLite file that opens as a normal
//! store. It is the way back to one machine, and a backup. With `--url`
//! absent it reads `store.remote_url`, so it also works after the key is
//! unset.

use crate::store_remote::{Remote, SqlValue};
use clap::Parser;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// Rows per request. sqld refuses one oversized reply, so reads page.
const PAGE: i64 = 200;

#[derive(Parser)]
#[command(
    name = "store-export",
    about = "Copy the shared primary into a local store file"
)]
struct Args {
    /// The file to write. It must not exist.
    #[arg(long)]
    out: PathBuf,
    /// The primary's URL. Defaults to store.remote_url.
    #[arg(long)]
    url: Option<String>,
}

pub fn run_store_export(args: &[String]) -> i32 {
    let args = match Args::try_parse_from(
        std::iter::once("store-export".to_string()).chain(args.iter().cloned()),
    ) {
        Ok(args) => args,
        Err(error) => {
            let _ = error.print();
            return 2;
        }
    };
    let remote = match args.url {
        Some(url) => Remote::parse(&url, None),
        None => crate::store_remote::configured()
            .and_then(|r| r.ok_or_else(|| "store.remote_url is unset; pass --url".to_string())),
    };
    match remote.and_then(|remote| export(&remote, &args.out)) {
        Ok(receipt) => {
            println!("{receipt}");
            0
        }
        Err(error) => {
            eprintln!("store-export: {error}");
            1
        }
    }
}

pub(crate) fn export(remote: &Remote, out: &Path) -> Result<Value, String> {
    if out.exists() {
        return Err(format!("{} exists; choose a new file", out.display()));
    }
    let partial = out.with_extension("partial");
    let _ = std::fs::remove_file(&partial);
    let schema = remote.execute(
        "SELECT type, name, sql FROM sqlite_master
         WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' AND name NOT LIKE 'libsql_%'
         ORDER BY CASE type WHEN 'table' THEN 0 WHEN 'index' THEN 1 WHEN 'view' THEN 2 ELSE 3 END, name",
        &[],
    )?;
    let mut connection = crate::store_conn::open_write(&partial)?;
    let transaction = connection.transaction().map_err(|e| e.to_string())?;
    let mut counts = Map::new();
    let field = |row: &[SqlValue], i: usize| row[i].text().unwrap_or("").to_string();
    for row in schema.rows.iter().filter(|row| field(row, 0) == "table") {
        let (name, sql) = (field(row, 1), field(row, 2));
        // A virtual table makes its own shadow tables, and its rows live in
        // them: create it, copy nothing through it, and copy the shadows raw.
        let virtual_table = sql.to_ascii_uppercase().starts_with("CREATE VIRTUAL TABLE");
        if let Err(error) = transaction.execute_batch(&sql) {
            if !error.to_string().contains("already exists") {
                return Err(format!("{name}: {error}"));
            }
        }
        if !virtual_table {
            let copied = copy_rows(remote, &transaction, &name)?;
            counts.insert(name, json!(copied));
        }
    }
    for row in schema.rows.iter().filter(|row| field(row, 0) != "table") {
        transaction
            .execute_batch(&field(row, 2))
            .map_err(|e| format!("{}: {e}", field(row, 1)))?;
    }
    transaction.commit().map_err(|e| e.to_string())?;
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")
        .map_err(|e| e.to_string())?;
    drop(connection);
    std::fs::rename(&partial, out).map_err(|e| format!("{}: {e}", out.display()))?;
    Ok(json!({"out": out, "from": remote.url(), "rows": counts}))
}

fn copy_rows(remote: &Remote, into: &rusqlite::Connection, table: &str) -> Result<u64, String> {
    let quoted = format!("\"{}\"", table.replace('"', "\"\""));
    let (mut copied, mut last, mut by_rowid) = (0u64, i64::MIN, true);
    loop {
        // A WITHOUT ROWID table (an FTS5 shadow table, say) pages by offset.
        let page = if by_rowid {
            let read = remote.execute(
                &format!("SELECT rowid AS fno_export_rowid, * FROM {quoted} WHERE rowid > ?1 ORDER BY rowid LIMIT {PAGE}"),
                &[SqlValue::Integer(last)],
            );
            match read {
                Err(error) if copied == 0 && error.contains("no such column: rowid") => {
                    by_rowid = false;
                    continue;
                }
                read => read?,
            }
        } else {
            remote.execute(
                &format!("SELECT * FROM {quoted} LIMIT {PAGE} OFFSET {copied}"),
                &[],
            )?
        };
        let Some(final_row) = page.rows.last() else {
            return Ok(copied);
        };
        let skip = usize::from(by_rowid);
        if by_rowid {
            last = final_row[0].integer().ok_or("rowid is not an integer")?;
        }
        let columns = &page.columns[skip..];
        let insert = format!(
            "INSERT OR REPLACE INTO {quoted} ({}) VALUES ({})",
            columns
                .iter()
                .map(|c| format!("\"{}\"", c.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(","),
            (1..=columns.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let mut statement = into.prepare_cached(&insert).map_err(|e| e.to_string())?;
        for row in &page.rows {
            statement
                .execute(rusqlite::params_from_iter(&row[skip..]))
                .map_err(|e| format!("{table}: {e}"))?;
            copied += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every table on the primary lands in a new file that opens as a local
    /// claims store, virtual tables included, and a second export refuses
    /// to overwrite it.
    #[test]
    fn store_export_copies_the_primary_into_a_store_that_opens_locally() {
        let root = tempfile::tempdir().unwrap();
        let dir = crate::claims::claims_dir_for(Some(root.path())).unwrap();
        let primary = crate::store_remote::test_primary::start();
        crate::claim_store::route_to_primary(Some((primary.remote.clone(), dir.clone())));
        let opts = crate::claims::AcquireOpts {
            root: Some(root.path().to_path_buf()),
            pid: Some(std::process::id()),
            ttl_ms: Some(60_000),
            ..Default::default()
        };
        let outcome = crate::claims::acquire("node:kept", "a", opts);
        assert!(matches!(outcome, crate::claims::AcquireOutcome::Acquired(_)), "{outcome:?}");
        crate::claim_store::route_to_primary(None);
        primary
            .db
            .lock()
            .unwrap()
            .execute_batch("CREATE VIRTUAL TABLE notes USING fts5(body); INSERT INTO notes VALUES ('hello primary');")
            .unwrap();

        let out = crate::claim_store::database_path_from_directory(&dir).unwrap();
        std::fs::create_dir_all(out.parent().unwrap()).unwrap();
        let receipt = export(&primary.remote, &out).unwrap();
        assert_eq!(receipt["rows"]["claims"], 1, "{receipt}");
        assert!(export(&primary.remote, &out).unwrap_err().contains("exists"));
        let body: String = crate::store_conn::open_read(&out)
            .unwrap()
            .query_row("SELECT body FROM notes WHERE notes MATCH 'primary'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(body, "hello primary");
        let kept = crate::claim_store::read("node:kept", Some(root.path())).unwrap().unwrap();
        assert_eq!(kept.holder, "a");
    }
}
