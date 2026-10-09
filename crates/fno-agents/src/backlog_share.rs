//! The shared backlog: with `store.remote_url` and `store.share_backlog` set,
//! the backlog tables live on the shared primary and this machine's graph.db
//! is a replica of them.
//!
//! Write: every backlog write opens through `backlog::open_connection`, which
//! calls [`attach`]. TEMP triggers record each row change to a shared
//! table. At commit, the changes go to the primary in one request, as one
//! transaction of conditional statements: each update and delete matches
//! every old column, each insert must not collide. A row that changed on the
//! primary since this replica read it refuses the whole write, and the local
//! transaction rolls back. So the primary decides, and no machine overwrites
//! a peer's change it never saw.
//!
//! Read: reads stay local. One daemon arm per machine runs [`sync`], which
//! applies the primary's change log to the replica.
//!
//! Unset, [`attach`] returns at once: no hook, no socket.

use crate::store_remote::{Remote, SqlValue};
use rusqlite::config::DbConfig;
use rusqlite::functions::FunctionFlags;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The backlog tables. They live on the primary when sharing is on.
pub(crate) const SHARED_TABLES: &[&str] = &[
    "nodes",
    "nodes_raw",
    "node_dispatch",
    "node_provenance",
    "supersessions",
    "sessions",
    "comments",
    "encounters",
    "findings",
    "pull_requests",
    "relations",
    "relations_unresolved",
    "node_costs",
    "decisions",
    "node_decisions",
    "edges",
    "harnesses",
    "models",
    "agent_sessions",
];

/// Tables in graph.db that stay on this machine: store metadata (the content
/// version, the sync cursor, render marks), the claims (their shared keys
/// have their own path), and the search index, which each replica rebuilds.
pub(crate) const LOCAL_TABLES: &[&str] = &[
    "graph_meta",
    "nodes_fts",
    "nodes_fts_data",
    "nodes_fts_idx",
    "nodes_fts_docsize",
    "nodes_fts_config",
];

/// A refused write's text starts with this, so the locked mutate retries
/// it as a conflict.
pub const REFUSED: &str = "backlog write refused by the shared primary";

/// A refusal carrying this names a row a peer changed first: a conflict to
/// retry, not a fault.
pub const CHANGED: &str = "changed on the primary after this machine read it";

const CURSOR: &str = "backlog_share_cursor";
const PAGE: i64 = 200;
pub const INTERVAL: Duration = Duration::from_secs(5);

/// The primary-side bookkeeping. `backlog_cas.n` takes only 1, so a
/// conditional statement that matched no row fails its batch. The seed
/// marker exists only once the seed finished, so a write to a half-seeded
/// primary refuses.
const PRIMARY_DDL: &str = "
CREATE TABLE IF NOT EXISTS backlog_cas (n INTEGER NOT NULL CHECK (n = 1));
CREATE TABLE IF NOT EXISTS backlog_changes (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  origin TEXT NOT NULL,
  at INTEGER NOT NULL,
  ops TEXT NOT NULL
);";

#[derive(Clone, Copy, Debug, PartialEq)]
enum Op {
    Insert,
    Update,
    Delete,
}

impl Op {
    fn code(self) -> &'static str {
        match self {
            Op::Insert => "i",
            Op::Update => "u",
            Op::Delete => "d",
        }
    }

    fn parse(code: &str) -> Option<Self> {
        match code {
            "i" => Some(Op::Insert),
            "u" => Some(Op::Update),
            "d" => Some(Op::Delete),
            _ => None,
        }
    }
}

/// One row change: the table, the column names, and the row before and
/// after. An insert has no old row, a delete no new one.
#[derive(Clone, Debug, PartialEq)]
struct Change {
    op: Op,
    table: String,
    columns: Vec<String>,
    old: Vec<SqlValue>,
    new: Vec<SqlValue>,
}

#[cfg(test)]
thread_local! {
    static TEST_PRIMARY: std::cell::RefCell<Option<(Remote, std::path::PathBuf)>> =
        const { std::cell::RefCell::new(None) };
}

/// Test seam: share the store at `db` (a graph.db path) with `remote` on
/// this thread.
#[cfg(test)]
pub(crate) fn route_to_primary(route: Option<(Remote, std::path::PathBuf)>) {
    TEST_PRIMARY.with(|p| *p.borrow_mut() = route);
}

thread_local! {
    static REFUSAL: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// The reason the last commit on this thread was refused, if it was.
pub(crate) fn take_refusal() -> Option<String> {
    REFUSAL.with(|r| r.borrow_mut().take())
}

/// The primary that holds the backlog behind `graph`. Only this machine's
/// configured graph is shared; a space, a sandbox or a fixture never dials
/// out.
pub(crate) fn primary_for(graph: &Path) -> Result<Option<Remote>, String> {
    let db = crate::backlog::database_path(graph);
    #[cfg(test)]
    if let Some((remote, at)) = TEST_PRIMARY.with(|p| p.borrow().clone()) {
        return Ok((at == db).then_some(remote));
    }
    let Some(remote) = crate::store_remote::share_backlog()? else {
        return Ok(None);
    };
    let configured = crate::backlog::database_path(&crate::backlog::settings::graph_path());
    Ok((configured == db).then_some(remote))
}

/// Column names per shared table, in table order.
fn shared_columns(connection: &Connection) -> Result<BTreeMap<String, Vec<String>>, String> {
    let mut out = BTreeMap::new();
    for table in SHARED_TABLES {
        let mut statement = connection
            .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
            .map_err(|e| e.to_string())?;
        let columns = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        if !columns.is_empty() {
            out.insert(table.to_string(), columns);
        }
    }
    Ok(out)
}

/// Install the write path on a backlog connection when sharing is on.
///
/// TEMP triggers on each shared table hand every row change to a Rust
/// function on this connection. They live in this connection only, so the
/// file's schema never changes. (The preupdate hook and the session
/// extension would capture the same rows, but either one makes the SQLite
/// build need libclang through bindgen, on every machine, key on or off.)
pub(crate) fn attach(connection: &Connection, graph: &Path) -> Result<(), String> {
    let Some(remote) = primary_for(graph)? else {
        return Ok(());
    };
    let columns = shared_columns(connection)?;
    let pending: Arc<Mutex<Vec<Change>>> = Arc::default();

    let record = Arc::clone(&pending);
    let names = columns.clone();
    connection
        .create_scalar_function(
            "fno_backlog_change",
            -1,
            FunctionFlags::SQLITE_UTF8,
            move |ctx| {
                let text = |i: usize| ctx.get_raw(i).as_str().unwrap_or("").to_string();
                let (table, op) = (text(0), text(1));
                let (Some(columns), Some(op)) = (names.get(&table), Op::parse(&op)) else {
                    return Ok(0);
                };
                let values: Vec<SqlValue> = (2..ctx.len())
                    .map(|i| SqlValue::from(ctx.get_raw(i)))
                    .collect();
                let (old, new) = match op {
                    Op::Insert => (Vec::new(), values),
                    Op::Delete => (values, Vec::new()),
                    Op::Update => {
                        let (old, new) = values.split_at(columns.len());
                        (old.to_vec(), new.to_vec())
                    }
                };
                record
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(Change {
                        op,
                        table,
                        columns: columns.clone(),
                        old,
                        new,
                    });
                Ok(0)
            },
        )
        .map_err(|e| e.to_string())?;
    let mut triggers = String::new();
    for (table, columns) in &columns {
        let row = |side: &str| {
            columns
                .iter()
                .map(|c| format!("{side}.\"{c}\""))
                .collect::<Vec<_>>()
                .join(", ")
        };
        for (op, event, values) in [
            ("i", "INSERT", row("NEW")),
            ("u", "UPDATE", format!("{}, {}", row("OLD"), row("NEW"))),
            ("d", "DELETE", row("OLD")),
        ] {
            triggers.push_str(&format!(
                "CREATE TEMP TRIGGER IF NOT EXISTS fno_share_{table}_{op} AFTER {event} ON main.\"{table}\" \
                 BEGIN SELECT fno_backlog_change('{table}', '{op}', {values}); END;\n"
            ));
        }
    }
    connection
        .execute_batch(&triggers)
        .map_err(|e| e.to_string())?;

    let send = Arc::clone(&pending);
    let origin = crate::claims::machine_id();
    connection
        .commit_hook(Some(move || {
            // A refusal belongs to this commit only: an earlier one left
            // unread must not explain a later, unrelated failure.
            REFUSAL.with(|r| r.borrow_mut().take());
            let changes = net(std::mem::take(
                &mut *send.lock().unwrap_or_else(|e| e.into_inner()),
            ));
            if changes.is_empty() {
                return false;
            }
            match publish(&remote, &origin, &changes) {
                Ok(()) => false,
                Err(reason) => {
                    eprintln!("{reason}");
                    REFUSAL.with(|r| *r.borrow_mut() = Some(reason));
                    // True turns this commit into a rollback.
                    true
                }
            }
        }))
        .map_err(|e| e.to_string())?;

    let clear = Arc::clone(&pending);
    connection
        .rollback_hook(Some(move || {
            clear.lock().unwrap_or_else(|e| e.into_inner()).clear();
        }))
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// One row's identity for chaining: its table and every value.
fn row_key(table: &str, row: &[SqlValue]) -> String {
    let values = row.iter().map(SqlValue::to_hrana).collect::<Vec<_>>();
    format!("{table}\u{1f}{}", Value::Array(values))
}

/// Fold one transaction's trigger records into one change per row: the row
/// as it was before the transaction and as it is after. Triggers can record
/// a nested change (a touch trigger's update) before the change that caused
/// it, so the fold chains records by value, each one's new row being the
/// next one's old row, instead of trusting the record order. Deletes go
/// first, then updates, then inserts, so a key freed by one row is free
/// before another row takes it.
fn net(changes: Vec<Change>) -> Vec<Change> {
    let mut by_old: std::collections::HashMap<String, Vec<usize>> = Default::default();
    let mut news = std::collections::HashSet::new();
    for (i, change) in changes.iter().enumerate() {
        if !change.old.is_empty() {
            by_old
                .entry(row_key(&change.table, &change.old))
                .or_default()
                .push(i);
        }
        if !change.new.is_empty() {
            news.insert(row_key(&change.table, &change.new));
        }
    }
    let mut used = vec![false; changes.len()];
    let is_head = |c: &Change| c.old.is_empty() || !news.contains(&row_key(&c.table, &c.old));
    let order = (0..changes.len())
        .filter(|&i| is_head(&changes[i]))
        .chain(0..changes.len());
    let mut out = Vec::new();
    for head in order {
        if used[head] {
            continue;
        }
        used[head] = true;
        let mut tail = head;
        while !changes[tail].new.is_empty() {
            let key = row_key(&changes[tail].table, &changes[tail].new);
            let next = by_old
                .get(&key)
                .and_then(|ids| ids.iter().copied().find(|&j| !used[j]));
            let Some(next) = next else { break };
            used[next] = true;
            tail = next;
        }
        let (old, new) = (changes[head].old.clone(), changes[tail].new.clone());
        let op = match (old.is_empty(), new.is_empty()) {
            (true, true) => continue,
            (true, false) => Op::Insert,
            (false, true) => Op::Delete,
            (false, false) if old == new => continue,
            (false, false) => Op::Update,
        };
        out.push(Change {
            op,
            old,
            new,
            ..changes[head].clone()
        });
    }
    out.sort_by_key(|c| match c.op {
        Op::Delete => 0,
        Op::Update => 1,
        Op::Insert => 2,
    });
    out
}

/// `col IS ?n AND ...` over every column, numbered from `first`.
fn match_all(columns: &[String], first: usize) -> String {
    columns
        .iter()
        .enumerate()
        .map(|(i, c)| format!("\"{c}\" IS ?{}", first + i))
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn insert_sql(table: &str, columns: &[String]) -> String {
    let names = columns
        .iter()
        .map(|c| format!("\"{c}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let marks = (1..=columns.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("INSERT INTO \"{table}\" ({names}) VALUES ({marks})")
}

/// The statement that applies `change` only when the row is still as this
/// replica saw it.
fn conditional(change: &Change) -> (String, Vec<SqlValue>) {
    let (table, columns) = (&change.table, &change.columns);
    match change.op {
        Op::Insert => (insert_sql(table, columns), change.new.clone()),
        Op::Delete => (
            format!("DELETE FROM \"{table}\" WHERE {}", match_all(columns, 1)),
            change.old.clone(),
        ),
        Op::Update => {
            let set = columns
                .iter()
                .enumerate()
                .map(|(i, c)| format!("\"{c}\" = ?{}", i + 1))
                .collect::<Vec<_>>()
                .join(", ");
            let mut args = change.new.clone();
            args.extend(change.old.iter().cloned());
            (
                format!(
                    "UPDATE \"{table}\" SET {set} WHERE {}",
                    match_all(columns, columns.len() + 1)
                ),
                args,
            )
        }
    }
}

fn ops_json(changes: &[Change]) -> String {
    let values = |row: &[SqlValue]| row.iter().map(SqlValue::to_hrana).collect::<Vec<_>>();
    Value::Array(
        changes
            .iter()
            .map(|c| {
                json!({"op": c.op.code(), "t": c.table, "c": c.columns,
                       "o": values(&c.old), "n": values(&c.new)})
            })
            .collect(),
    )
    .to_string()
}

fn parse_ops(text: &str) -> Result<Vec<Change>, String> {
    let rows: Vec<Value> = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let values = |v: &Value| -> Result<Vec<SqlValue>, String> {
        v.as_array()
            .into_iter()
            .flatten()
            .map(SqlValue::from_hrana)
            .collect()
    };
    rows.iter()
        .map(|row| {
            Ok(Change {
                op: row["op"]
                    .as_str()
                    .and_then(Op::parse)
                    .ok_or("change without an op")?,
                table: row["t"].as_str().ok_or("change without a table")?.into(),
                columns: row["c"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|c| c.as_str().map(str::to_string))
                    .collect(),
                old: values(&row["o"])?,
                new: values(&row["n"])?,
            })
        })
        .collect()
}

/// Send one write to the primary: every change as a conditional statement
/// with its row-count check, then the change-log row, in one request.
fn publish(remote: &Remote, origin: &str, changes: &[Change]) -> Result<(), String> {
    let mut steps = vec![(
        "SELECT 1 FROM backlog_seed".to_string(),
        Vec::<SqlValue>::new(),
    )];
    let mut owner = vec![None];
    for (i, change) in changes.iter().enumerate() {
        steps.push(conditional(change));
        owner.push(Some(i));
        steps.push((
            "INSERT INTO backlog_cas (n) SELECT changes()".to_string(),
            Vec::new(),
        ));
        owner.push(Some(i));
    }
    steps.push(("DELETE FROM backlog_cas".to_string(), Vec::new()));
    owner.push(None);
    let at = crate::backlog::now_ms() as i64;
    steps.push((
        "INSERT INTO backlog_changes (origin, at, ops) VALUES (?1, ?2, ?3)".to_string(),
        vec![
            SqlValue::Text(origin.to_string()),
            SqlValue::Integer(at),
            SqlValue::Text(ops_json(changes)),
        ],
    ));
    owner.push(None);
    remote.transaction(&steps).map(drop).map_err(|(step, error)| {
        let hint = if error.contains("no such table: backlog_seed") {
            " The primary holds no seeded backlog: run `fno agents claim backlog seed` on one machine first.".to_string()
        } else if crate::store_remote::is_unreachable(&error) {
            String::new()
        } else {
            match step.and_then(|s| owner.get(s).copied().flatten()) {
                Some(i) => format!(
                    " The {} row {CHANGED}. Run `fno agents claim backlog sync` (the daemon does it every 5 s), then retry.",
                    changes[i].table
                ),
                None => String::new(),
            }
        };
        format!("{REFUSED}: {error}.{hint} Nothing was written locally.")
    })
}

/// What one sync did.
#[derive(Debug, Default, PartialEq, serde::Serialize)]
pub struct Receipt {
    pub shared: bool,
    pub snapshot: bool,
    pub applied: usize,
    pub cursor: i64,
}

/// The replica's connection: no hooks, no triggers (the change log already
/// carries every row a trigger made), no foreign-key actions.
fn replica(graph: &Path) -> Result<Connection, String> {
    drop(crate::backlog::open(graph)?);
    let connection = crate::store_conn::open_write(&crate::backlog::database_path(graph))?;
    connection
        .set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_TRIGGER, false)
        .map_err(|e| e.to_string())?;
    connection
        .execute_batch("PRAGMA foreign_keys=OFF;")
        .map_err(|e| e.to_string())?;
    Ok(connection)
}

/// Bring the replica up to the primary. A replica that never synced takes
/// a full snapshot first.
pub fn sync(graph: &Path) -> Result<Receipt, String> {
    let Some(remote) = primary_for(graph)? else {
        return Ok(Receipt::default());
    };
    let mut connection = replica(graph)?;
    let cursor = crate::backlog::meta(&connection, CURSOR)?.and_then(|v| v.parse::<i64>().ok());
    let Some(cursor) = cursor else {
        return snapshot(&remote, graph, &mut connection);
    };
    catch_up(&remote, &mut connection, cursor)
}

/// Apply the change log past `cursor`.
fn catch_up(
    remote: &Remote,
    connection: &mut Connection,
    mut cursor: i64,
) -> Result<Receipt, String> {
    let mut receipt = Receipt {
        shared: true,
        cursor,
        ..Receipt::default()
    };
    loop {
        let page = remote.execute(
            "SELECT seq, ops FROM backlog_changes WHERE seq > ?1 ORDER BY seq LIMIT ?2",
            &[SqlValue::Integer(cursor), SqlValue::Integer(PAGE)],
        )?;
        if page.rows.is_empty() {
            return Ok(receipt);
        }
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        let mut nodes_moved = false;
        for row in &page.rows {
            let seq = row[0].integer().ok_or("change row without a seq")?;
            for change in parse_ops(row[1].text().unwrap_or("[]"))? {
                match apply(&transaction, &change)? {
                    Applied::Done => {
                        receipt.applied += 1;
                        nodes_moved |= change.table == "nodes";
                    }
                    Applied::Present => {}
                }
            }
            cursor = seq;
        }
        finish(&transaction, cursor, nodes_moved)?;
        transaction.commit().map_err(|e| e.to_string())?;
        receipt.cursor = cursor;
    }
}

enum Applied {
    Done,
    /// The replica already holds this change (its own write, or a replay).
    Present,
}

fn row_exists(connection: &Connection, change: &Change, row: &[SqlValue]) -> Result<bool, String> {
    connection
        .query_row(
            &format!(
                "SELECT 1 FROM \"{}\" WHERE {} LIMIT 1",
                change.table,
                match_all(&change.columns, 1)
            ),
            rusqlite::params_from_iter(row),
            |_| Ok(()),
        )
        .optional()
        .map(|found| found.is_some())
        .map_err(|e| format!("{}: {e}", change.table))
}

fn apply(connection: &Connection, change: &Change) -> Result<Applied, String> {
    if !SHARED_TABLES.contains(&change.table.as_str()) {
        return Err(format!(
            "the change log names a local table {}",
            change.table
        ));
    }
    let present = |row: &[SqlValue]| row_exists(connection, change, row);
    let run = |sql: &str, args: &[SqlValue]| {
        connection
            .execute(sql, rusqlite::params_from_iter(args))
            .map_err(|e| format!("{}: {e}", change.table))
    };
    match change.op {
        Op::Insert | Op::Update if present(&change.new)? => return Ok(Applied::Present),
        Op::Delete if !present(&change.old)? => return Ok(Applied::Present),
        Op::Insert => {}
        Op::Update | Op::Delete => {
            let (sql, args) = conditional(&Change {
                op: Op::Delete,
                ..change.clone()
            });
            run(&sql, &args)?;
        }
    }
    if change.op != Op::Delete {
        let sql =
            insert_sql(&change.table, &change.columns).replacen("INSERT", "INSERT OR REPLACE", 1);
        run(&sql, &change.new)?;
    }
    Ok(Applied::Done)
}

/// Stamp the cursor and a fresh content version, so a locked mutate that
/// read before this sync sees a conflict and re-reads. Rebuild the search
/// index when nodes moved, since triggers were off.
fn finish(connection: &Connection, cursor: i64, nodes_moved: bool) -> Result<(), String> {
    crate::backlog::stamp_meta(connection, CURSOR, &cursor.to_string())?;
    if nodes_moved {
        connection
            .execute("INSERT INTO nodes_fts(nodes_fts) VALUES('rebuild')", [])
            .map_err(|e| e.to_string())?;
    }
    crate::backlog::stamp_version(
        connection,
        &format!("sqlite:sync-{cursor}-{}", crate::backlog::now_ms()),
    )
}

/// Replace every shared table with the primary's rows. The first snapshot
/// keeps a backup copy of the replica beside it, because this machine's
/// own backlog rows are replaced.
fn snapshot(remote: &Remote, graph: &Path, connection: &mut Connection) -> Result<Receipt, String> {
    let top = remote
        .execute("SELECT COALESCE(MAX(seq), 0) FROM backlog_changes", &[])?
        .rows
        .first()
        .and_then(|r| r[0].integer())
        .unwrap_or(0);
    if crate::backlog::meta(connection, CURSOR)?.is_none() {
        let db = crate::backlog::database_path(graph);
        let backup = db.with_extension(format!("pre-share-{}.db", crate::backlog::now_ms()));
        let mut copy = Connection::open(&backup).map_err(|e| e.to_string())?;
        rusqlite::backup::Backup::new(connection, &mut copy)
            .and_then(|b| b.run_to_completion(256, Duration::ZERO, None))
            .map_err(|e| format!("backup {}: {e}", backup.display()))?;
    }
    let columns = shared_columns(connection)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| e.to_string())?;
    let mut applied = 0;
    for (table, columns) in &columns {
        transaction
            .execute(&format!("DELETE FROM \"{table}\""), [])
            .map_err(|e| format!("{table}: {e}"))?;
        let names = columns
            .iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let insert = insert_sql(table, columns);
        let mut after = 0;
        loop {
            let page = remote.execute(
                &format!(
                    "SELECT rowid, {names} FROM \"{table}\" WHERE rowid > ?1 ORDER BY rowid LIMIT ?2"
                ),
                &[SqlValue::Integer(after), SqlValue::Integer(PAGE)],
            )?;
            for row in &page.rows {
                after = row[0].integer().unwrap_or(after);
                transaction
                    .execute(&insert, rusqlite::params_from_iter(&row[1..]))
                    .map_err(|e| format!("{table}: {e}"))?;
                applied += 1;
            }
            if (page.rows.len() as i64) < PAGE {
                break;
            }
        }
    }
    finish(&transaction, top, true)?;
    transaction.commit().map_err(|e| e.to_string())?;
    // Rows written after `top` replay as present or apply.
    let caught = catch_up(remote, connection, top)?;
    Ok(Receipt {
        shared: true,
        snapshot: true,
        applied: applied + caught.applied,
        cursor: caught.cursor.max(top),
    })
}

/// Copy this replica's backlog into an empty primary. Refuses a primary
/// that already holds a seeded backlog.
pub fn seed(graph: &Path) -> Result<Value, String> {
    let remote = primary_for(graph)?
        .ok_or("set store.remote_url and store.share_backlog = true in the global config first")?;
    let seeded = remote.execute(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'backlog_seed'",
        &[],
    )?;
    if !seeded.rows.is_empty() {
        return Err(format!(
            "the primary {} already holds a seeded backlog; run `fno agents claim backlog sync` instead",
            remote.url()
        ));
    }
    let connection = replica(graph)?;
    let mut ddl = String::from(PRIMARY_DDL);
    {
        let mut statement = connection
            .prepare(
                "SELECT sql FROM sqlite_master WHERE sql IS NOT NULL
                 AND type IN ('table', 'index') AND tbl_name = ?1
                 ORDER BY CASE type WHEN 'table' THEN 0 ELSE 1 END",
            )
            .map_err(|e| e.to_string())?;
        for table in SHARED_TABLES {
            for sql in statement
                .query_map([table], |row| row.get::<_, String>(0))
                .map_err(|e| e.to_string())?
            {
                let sql = sql.map_err(|e| e.to_string())?;
                let sql = sql
                    .replacen("CREATE TABLE ", "CREATE TABLE IF NOT EXISTS ", 1)
                    .replacen("CREATE INDEX ", "CREATE INDEX IF NOT EXISTS ", 1)
                    .replacen(
                        "CREATE UNIQUE INDEX ",
                        "CREATE UNIQUE INDEX IF NOT EXISTS ",
                        1,
                    )
                    .replace("IF NOT EXISTS IF NOT EXISTS", "IF NOT EXISTS");
                ddl.push_str(&sql);
                ddl.push_str(";\n");
            }
        }
    }
    remote.script(&ddl)?;
    let columns = shared_columns(&connection)?;
    let mut counts = serde_json::Map::new();
    for (table, columns) in &columns {
        let insert = insert_sql(table, columns);
        let names = columns
            .iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let mut steps = vec![(format!("DELETE FROM \"{table}\""), Vec::new())];
        let mut statement = connection
            .prepare(&format!("SELECT {names} FROM \"{table}\""))
            .map_err(|e| e.to_string())?;
        let mut rows = statement.query([]).map_err(|e| e.to_string())?;
        let mut count = 0;
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let values = (0..columns.len())
                .map(|i| row.get_ref(i).map(SqlValue::from))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?;
            steps.push((insert.clone(), values));
            count += 1;
            if steps.len() as i64 >= PAGE {
                remote
                    .transaction(&std::mem::take(&mut steps))
                    .map_err(|(_, e)| format!("{table}: {e}"))?;
            }
        }
        if !steps.is_empty() {
            remote
                .transaction(&steps)
                .map_err(|(_, e)| format!("{table}: {e}"))?;
        }
        counts.insert(table.clone(), json!(count));
    }
    remote.script("CREATE TABLE backlog_seed (at INTEGER NOT NULL); INSERT INTO backlog_seed (at) VALUES (0);")?;
    let top = remote
        .execute("SELECT COALESCE(MAX(seq), 0) FROM backlog_changes", &[])?
        .rows
        .first()
        .and_then(|r| r[0].integer())
        .unwrap_or(0);
    crate::backlog::stamp_meta(&connection, CURSOR, &top.to_string())?;
    Ok(json!({"primary": remote.url(), "rows": counts, "cursor": top}))
}

/// `fno agents claim backlog seed|sync`.
pub fn run(args: &[String]) -> i32 {
    let graph = crate::backlog::settings::graph_path();
    let outcome = match args.first().map(String::as_str) {
        Some("seed") => seed(&graph),
        Some("sync") => {
            sync(&graph).and_then(|r| serde_json::to_value(r).map_err(|e| e.to_string()))
        }
        _ => {
            eprintln!(
                "usage: fno agents claim backlog seed|sync\n  seed  copy this machine's backlog into an empty shared primary\n  sync  bring this machine's replica up to the primary"
            );
            return 2;
        }
    };
    match outcome {
        Ok(receipt) => {
            println!("{receipt}");
            0
        }
        Err(error) => {
            eprintln!("claim backlog: {error}");
            1
        }
    }
}

/// The resident syncer: one sync every [`INTERVAL`], one in flight, off
/// the daemon's own loop.
#[derive(Default)]
pub struct Arm {
    last_tick: Mutex<Option<Instant>>,
    in_flight: Arc<AtomicBool>,
}

pub fn maybe_tick(arm: &Arm) {
    match crate::store_remote::share_backlog() {
        Ok(Some(_)) => {}
        Ok(None) => return,
        Err(error) => {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| eprintln!("backlog-share: {error}"));
            return;
        }
    }
    {
        let mut last = arm.last_tick.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|tick| tick.elapsed() < INTERVAL)
            || arm.in_flight.swap(true, Ordering::SeqCst)
        {
            return;
        }
        *last = Some(Instant::now());
    }
    let flag = Arc::clone(&arm.in_flight);
    std::thread::spawn(move || {
        if let Err(error) = sync(&crate::backlog::settings::graph_path()) {
            eprintln!("backlog-share: sync: {error}");
        }
        flag.store(false, Ordering::SeqCst);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store_remote::test_primary;
    use rusqlite::params;
    use std::path::PathBuf;

    struct Machine {
        _dir: tempfile::TempDir,
        graph: PathBuf,
    }

    fn machine() -> Machine {
        let dir = tempfile::tempdir().unwrap();
        let graph = dir.path().join("graph.json");
        drop(crate::backlog::open(&graph).unwrap());
        Machine { _dir: dir, graph }
    }

    fn on(primary: &test_primary::Primary, m: &Machine) {
        route_to_primary(Some((
            primary.remote.clone(),
            crate::backlog::database_path(&m.graph),
        )));
    }

    fn title(m: &Machine, id: &str) -> Option<String> {
        crate::backlog::open(&m.graph)
            .unwrap()
            .query_row("SELECT title FROM nodes WHERE id = ?1", [id], |r| r.get(0))
            .optional()
            .unwrap()
    }

    fn add_node(m: &Machine, id: &str, title: &str) -> Result<(), String> {
        let mut connection = crate::backlog::open(&m.graph)?;
        let transaction = connection.transaction().map_err(|e| e.to_string())?;
        transaction
            .execute(
                "INSERT INTO nodes (id, ordinal, slug, title, status, priority)
                 VALUES (?1, 1, ?1, 'draft', 'idea', 'p2')",
                params![id],
            )
            .map_err(|e| e.to_string())?;
        transaction
            .execute(
                "UPDATE nodes SET title = ?2 WHERE id = ?1",
                params![id, title],
            )
            .map_err(|e| e.to_string())?;
        transaction.commit().map_err(|e| e.to_string())
    }

    #[test]
    fn every_graph_table_is_shared_or_local() {
        let m = machine();
        let connection = crate::backlog::open(&m.graph).unwrap();
        let mut statement = connection
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            )
            .unwrap();
        let unclassified: Vec<String> = statement
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .filter(|t| !SHARED_TABLES.contains(&t.as_str()) && !LOCAL_TABLES.contains(&t.as_str()))
            .collect();
        assert!(
            unclassified.is_empty(),
            "classify these tables: {unclassified:?}"
        );
    }

    #[test]
    fn a_write_lands_on_the_primary_and_a_peer_syncs_it() {
        let primary = test_primary::start();
        let (a, b) = (machine(), machine());
        on(&primary, &a);
        seed(&a.graph).unwrap();
        add_node(&a, "x-1", "shared").unwrap();
        let on_primary: String = primary
            .db
            .lock()
            .unwrap()
            .query_row("SELECT title FROM nodes WHERE id = 'x-1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(on_primary, "shared");
        let log: i64 = primary
            .db
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM backlog_changes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(log, 1, "one write, one change-log row");

        on(&primary, &b);
        let receipt = sync(&b.graph).unwrap();
        assert!(receipt.snapshot);
        assert_eq!(title(&b, "x-1").as_deref(), Some("shared"));
        let found: i64 = crate::backlog::open(&b.graph)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM nodes_fts WHERE nodes_fts MATCH 'shared'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(found, 1, "the replica's search index sees the synced row");

        // A's own write replays on A and leaves the row as it was.
        on(&primary, &a);
        sync(&a.graph).unwrap();
        assert_eq!(title(&a, "x-1").as_deref(), Some("shared"));
        route_to_primary(None);
    }

    #[test]
    fn a_stale_or_unreachable_write_is_refused_and_rolled_back() {
        let primary = test_primary::start();
        let (a, b) = (machine(), machine());
        on(&primary, &a);
        seed(&a.graph).unwrap();
        add_node(&a, "x-1", "first").unwrap();
        on(&primary, &b);
        sync(&b.graph).unwrap();
        on(&primary, &a);
        crate::backlog::open(&a.graph)
            .unwrap()
            .execute("UPDATE nodes SET title = 'from a' WHERE id = 'x-1'", [])
            .unwrap();

        on(&primary, &b);
        assert!(crate::backlog::open(&b.graph)
            .unwrap()
            .execute("UPDATE nodes SET title = 'from b' WHERE id = 'x-1'", [])
            .is_err());
        let reason = take_refusal().unwrap();
        assert!(
            reason.starts_with(REFUSED) && reason.contains("nodes row changed"),
            "{reason}"
        );
        assert_eq!(title(&b, "x-1").as_deref(), Some("first"));

        sync(&b.graph).unwrap();
        assert_eq!(title(&b, "x-1").as_deref(), Some("from a"));

        // A second seed refuses; an unreachable primary refuses the write.
        on(&primary, &a);
        assert!(seed(&a.graph).unwrap_err().contains("already holds"));
        route_to_primary(Some((
            test_primary::dead(),
            crate::backlog::database_path(&a.graph),
        )));
        assert!(add_node(&a, "x-2", "lost").is_err());
        assert!(take_refusal().unwrap().contains("is unreachable"));
        route_to_primary(None);
        assert_eq!(title(&a, "x-2"), None);
    }
}
