//! The shared backlog: with `store.remote_url` and `store.share_backlog` set,
//! the backlog tables live on the shared primary and this machine's graph.db
//! is a replica of them.
//!
//! Write: every backlog write opens through `backlog::open_connection`, which
//! calls [`attach`]. TEMP triggers copy each row change to a shared table into
//! the local `backlog_outbox`, in the writer's own transaction, so a commit
//! stays local and holds no lock across the network. [`flush`] then sends the
//! outbox to the primary in one request, under its own lock file, after the
//! graph lock is gone: one transaction of conditional statements, where each
//! update and delete matches every old column and each insert must not
//! collide. A row a peer changed first refuses the whole batch, and the
//! replica takes the primary's rows back for every row the batch touched.
//! So the primary decides, and no machine overwrites a peer's change it never
//! saw.
//!
//! Read: reads stay local. One daemon arm per machine runs [`flush`] and then
//! [`sync`], which applies the primary's change log to the replica.
//!
//! Guard: a store that shares carries a persistent trigger on each shared
//! table that calls `fno_backlog_writer()`. Only a build with this module
//! registers that function, so an older build's write to a shared table
//! fails at once instead of landing with no outbox record.
//!
//! Unset, [`attach`] drops any guard a past share left, so an older build
//! can write again, and opens no socket.

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

/// The backlog tables. They live on the primary when sharing is on. A
/// parent comes before every table that references it: the primary enforces
/// foreign keys, so seed and publish write in this order.
pub(crate) const SHARED_TABLES: &[&str] = &[
    "harnesses",
    "models",
    "agent_sessions",
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
];

/// The position of `table` in `SHARED_TABLES`.
fn rank(table: &str) -> usize {
    SHARED_TABLES
        .iter()
        .position(|t| *t == table)
        .unwrap_or(SHARED_TABLES.len())
}

/// Tables in graph.db that stay on this machine: store metadata (the content
/// version, the sync cursor, render marks), the outbox, and the search index,
/// which each replica rebuilds.
#[cfg(test)]
const LOCAL_TABLES: &[&str] = &[
    "graph_meta",
    "backlog_outbox",
    "nodes_fts",
    "nodes_fts_data",
    "nodes_fts_idx",
    "nodes_fts_docsize",
    "nodes_fts_config",
];

/// A refused flush's text starts with this.
pub const REFUSED: &str = "backlog write refused by the shared primary";

/// A refusal carrying this names a row a peer changed first: a conflict to
/// retry, not a fault.
pub const CHANGED: &str = "changed on the primary after this machine read it";

const CURSOR: &str = "backlog_share_cursor";
/// The last outbox id of a batch whose result never came back. The next
/// flush resends exactly that batch, so the primary can answer that it
/// already landed.
const SENDING: &str = "backlog_share_sending";
/// The function every guard trigger calls.
const WRITER: &str = "fno_backlog_writer";
const PAGE: i64 = 200;
pub const INTERVAL: Duration = Duration::from_secs(5);
const FLUSH_WAIT: Duration = Duration::from_secs(30);

const OUTBOX_DDL: &str = "CREATE TABLE IF NOT EXISTS main.backlog_outbox (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  change TEXT NOT NULL
);";

/// The primary-side bookkeeping. `backlog_cas.n` takes only 1, so a
/// conditional statement that matched no row fails its batch. A batch id
/// lands once, so a resent outbox never applies twice. The seed marker exists
/// only once the seed finished, so a write to a half-seeded primary refuses.
const PRIMARY_DDL: &str = "
CREATE TABLE IF NOT EXISTS backlog_cas (n INTEGER NOT NULL CHECK (n = 1));
CREATE TABLE IF NOT EXISTS backlog_changes (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  origin TEXT NOT NULL,
  batch TEXT NOT NULL UNIQUE,
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

impl Change {
    fn to_json(&self) -> Value {
        let values = |row: &[SqlValue]| row.iter().map(SqlValue::to_hrana).collect::<Vec<_>>();
        json!({"op": self.op.code(), "t": self.table, "c": self.columns,
               "o": values(&self.old), "n": values(&self.new)})
    }

    fn from_json(row: &Value) -> Result<Self, String> {
        let values = |v: &Value| -> Result<Vec<SqlValue>, String> {
            v.as_array()
                .into_iter()
                .flatten()
                .map(SqlValue::from_hrana)
                .collect()
        };
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
    }
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
        let columns = column_info(connection, table)?
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>();
        if !columns.is_empty() {
            out.insert(table.to_string(), columns);
        }
    }
    Ok(out)
}

/// `(name, primary-key position)` per column; position 0 is not in the key.
fn column_info(connection: &Connection, table: &str) -> Result<Vec<(String, i64)>, String> {
    let mut statement = connection
        .prepare(&format!(
            "SELECT name, pk FROM pragma_table_info('{table}')"
        ))
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string());
    rows
}

/// Mark `connection` as a writer that records its changes. Every store
/// connection this build opens calls it before its first write, key on or
/// off, so the guard triggers pass it.
pub(crate) fn register_writer(connection: &Connection) -> Result<(), String> {
    connection
        .create_scalar_function(WRITER, 0, FunctionFlags::SQLITE_DETERMINISTIC, |_| Ok(1))
        .map_err(|e| e.to_string())
}

/// Install the guard triggers on every shared table this store holds.
fn guard(connection: &Connection, columns: &BTreeMap<String, Vec<String>>) -> Result<(), String> {
    let mut ddl = String::new();
    for table in columns.keys() {
        for (op, event) in [("i", "INSERT"), ("u", "UPDATE"), ("d", "DELETE")] {
            ddl.push_str(&format!(
                "CREATE TRIGGER IF NOT EXISTS main.fno_share_guard_{table}_{op} BEFORE {event} \
                 ON \"{table}\" BEGIN SELECT {WRITER}(); END;\n"
            ));
        }
    }
    connection.execute_batch(&ddl).map_err(|e| e.to_string())
}

/// Drop the guard triggers: with sharing off, no write needs an outbox.
fn unguard(connection: &Connection) -> Result<(), String> {
    let names: Vec<String> = {
        let mut statement = connection
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'trigger' \
                 AND name LIKE 'fno\\_share\\_guard\\_%' ESCAPE '\\'",
            )
            .map_err(|e| e.to_string())?;
        let names = statement
            .query_map([], |row| row.get(0))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        names
    };
    let ddl: String = names
        .iter()
        .map(|name| format!("DROP TRIGGER IF EXISTS main.\"{name}\";\n"))
        .collect();
    connection.execute_batch(&ddl).map_err(|e| e.to_string())
}

/// Install the write path on a backlog connection when sharing is on.
///
/// TEMP triggers live in this connection only, so the shared tables keep
/// their schema. They hand each row to a Rust function, which keeps every
/// value's type, and write the result to the outbox in the same transaction.
/// (The preupdate hook and the session extension would capture the same
/// rows, but either one makes the SQLite build need libclang through
/// bindgen, on every machine, key on or off.)
pub(crate) fn attach(connection: &Connection, graph: &Path) -> Result<(), String> {
    if primary_for(graph)?.is_none() {
        // Only the key going off drops the guard. With the key on, a store
        // this process does not share may still be the shared one.
        return match crate::store_remote::share_backlog()? {
            None => unguard(connection),
            Some(_) => Ok(()),
        };
    }
    connection
        .execute_batch(OUTBOX_DDL)
        .map_err(|e| e.to_string())?;
    let columns = shared_columns(connection)?;
    guard(connection, &columns)?;
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
                    return Err(rusqlite::Error::UserFunctionError(
                        format!("fno_backlog_change: unknown table {table} or op {op}").into(),
                    ));
                };
                let values: Vec<SqlValue> = (2..ctx.len())
                    .map(|i| SqlValue::from(ctx.get_raw(i)))
                    .collect();
                let (old, new) = match op {
                    Op::Insert => (Vec::new(), values),
                    Op::Delete => (values, Vec::new()),
                    Op::Update => {
                        let (old, new) = values.split_at(columns.len().min(values.len()));
                        (old.to_vec(), new.to_vec())
                    }
                };
                let change = Change {
                    op,
                    table,
                    columns: columns.clone(),
                    old,
                    new,
                };
                Ok(change.to_json().to_string())
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
                 BEGIN INSERT INTO main.backlog_outbox (change) \
                 VALUES (fno_backlog_change('{table}', '{op}', {values})); END;\n"
            ));
        }
    }
    connection
        .execute_batch(&triggers)
        .map_err(|e| e.to_string())
}

/// One row's identity for chaining: its table and every value.
fn row_key(table: &str, row: &[SqlValue]) -> String {
    let values = row.iter().map(SqlValue::to_hrana).collect::<Vec<_>>();
    format!("{table}\u{1f}{}", Value::Array(values))
}

/// Fold the outbox into one change per row: the row as the primary last saw
/// it and as it is now. Triggers can record a nested change (a touch
/// trigger's update) before the change that caused it, so the fold chains
/// records by value, each one's new row being the next one's old row,
/// instead of trusting the record order. Deletes go first, then updates,
/// then inserts, so a key freed by one row is free before another row takes
/// it. Inserts go parent table first and deletes child table first.
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
        .chain(0..changes.len())
        .collect::<Vec<_>>();
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
        Op::Delete => (0, SHARED_TABLES.len() - rank(&c.table)),
        Op::Update => (1, 0),
        Op::Insert => (2, rank(&c.table)),
    });
    out
}

/// `col IS ?n AND ...` over `columns`, numbered from `first`.
fn match_all(columns: &[String], first: usize) -> String {
    columns
        .iter()
        .enumerate()
        .map(|(i, c)| format!("\"{c}\" IS ?{}", first + i))
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn quoted(columns: &[String]) -> String {
    columns
        .iter()
        .map(|c| format!("\"{c}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

fn insert_sql(table: &str, columns: &[String]) -> String {
    let marks = (1..=columns.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "INSERT INTO \"{table}\" ({}) VALUES ({marks})",
        quoted(columns)
    )
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
    Value::Array(changes.iter().map(Change::to_json).collect()).to_string()
}

fn parse_ops(text: &str) -> Result<Vec<Change>, String> {
    let rows: Vec<Value> = serde_json::from_str(text).map_err(|e| e.to_string())?;
    rows.iter().map(Change::from_json).collect()
}

/// Send one outbox batch: every change as a conditional statement with its
/// row-count check, then the change-log row, in one request.
fn publish(remote: &Remote, origin: &str, batch: &str, changes: &[Change]) -> Result<(), String> {
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
    steps.push((
        "INSERT INTO backlog_changes (origin, batch, at, ops) VALUES (?1, ?2, ?3, ?4)".to_string(),
        vec![
            SqlValue::Text(origin.to_string()),
            SqlValue::Text(batch.to_string()),
            SqlValue::Integer(crate::backlog::now_ms() as i64),
            SqlValue::Text(ops_json(changes)),
        ],
    ));
    owner.push(None);
    remote.transaction(&steps).map(drop).map_err(|(step, error)| {
        let hint = if unseeded(&error) {
            " The primary holds no seeded backlog: run `fno agents claim backlog seed` on one machine first.".to_string()
        } else if crate::store_remote::is_unreachable(&error) || landed_before(&error) {
            String::new()
        } else {
            match step.and_then(|s| owner.get(s).copied().flatten()) {
                Some(i) => format!(
                    " The {} row {CHANGED}. This machine took the primary's rows back; retry the write.",
                    changes[i].table
                ),
                None => String::new(),
            }
        };
        format!("{REFUSED}: {error}.{hint}")
    })
}

fn unseeded(error: &str) -> bool {
    error.contains("no such table: backlog_seed")
}

/// The batch id is already on the primary: an earlier flush landed it and
/// died before it cleared the outbox.
fn landed_before(error: &str) -> bool {
    error.contains("UNIQUE constraint failed: backlog_changes.batch")
}

/// The replica's connection: no TEMP triggers, no main-schema triggers (the
/// change log already carries every row a trigger made), no foreign-key
/// actions. It never takes the graph lock; only a store that does not exist
/// yet goes through the full open once.
fn replica(graph: &Path) -> Result<Connection, String> {
    let db = crate::backlog::database_path(graph);
    if !db.exists() {
        drop(crate::backlog::open(graph)?);
    }
    let connection = crate::store_conn::open_write(&db)?;
    register_writer(&connection)?;
    connection
        .set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_TRIGGER, false)
        .map_err(|e| e.to_string())?;
    connection
        .execute_batch(&format!("PRAGMA foreign_keys=OFF;\n{OUTBOX_DDL}"))
        .map_err(|e| e.to_string())?;
    Ok(connection)
}

/// Send this machine's outbox to the primary. Runs under its own lock file,
/// never the graph lock, so a slow primary holds up no writer. `Ok(n)` is the
/// number of row changes the primary took. A refusal takes the primary's
/// rows back for every row the batch touched, clears the batch, and returns
/// the refusal. An unreachable or unseeded primary keeps the outbox for the
/// next flush.
pub fn flush(graph: &Path) -> Result<usize, String> {
    let Some(remote) = primary_for(graph)? else {
        return Ok(0);
    };
    let db = crate::backlog::database_path(graph);
    let _lock = crate::graph_store::BoundedLock::acquire(&db.with_extension("share"), FLUSH_WAIT)
        .map_err(|e| e.to_string())?;
    let mut connection = replica(graph)?;
    let mut sending =
        crate::backlog::meta(&connection, SENDING)?.and_then(|v| v.parse::<i64>().ok());
    if let Some(mark) = sending {
        let held: bool = connection
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM backlog_outbox WHERE id <= ?1)",
                [mark],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        if !held {
            connection
                .execute("DELETE FROM graph_meta WHERE key = ?1", [SENDING])
                .map_err(|e| e.to_string())?;
            sending = None;
        }
    }
    let rows: Vec<(i64, String)> = {
        let mut statement = connection
            .prepare("SELECT id, change FROM backlog_outbox WHERE id <= ?1 ORDER BY id")
            .map_err(|e| e.to_string())?;
        let rows = statement
            .query_map([sending.unwrap_or(i64::MAX)], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        rows
    };
    let Some(&(last, _)) = rows.last() else {
        return Ok(0);
    };
    let changes = net(rows
        .iter()
        .map(|(_, text)| {
            serde_json::from_str(text)
                .map_err(|e| e.to_string())
                .and_then(|v| Change::from_json(&v))
        })
        .collect::<Result<Vec<_>, _>>()?);
    let clear = |connection: &Connection| {
        connection
            .execute_batch(&format!(
                "DELETE FROM backlog_outbox WHERE id <= {last};
                 DELETE FROM graph_meta WHERE key = '{SENDING}';"
            ))
            .map_err(|e| e.to_string())
    };
    if changes.is_empty() {
        clear(&connection)?;
        return Ok(0);
    }
    let origin = crate::claims::machine_id();
    // The content hash keeps a reused outbox id (a rebuilt or restored
    // store) from reading as a batch that already landed.
    let batch = format!(
        "{origin}:{}:{last}:{:016x}",
        db.display(),
        fnv1a(ops_json(&changes).as_bytes())
    );
    crate::backlog::stamp_meta(&connection, SENDING, &last.to_string())?;
    match publish(&remote, &origin, &batch, &changes) {
        Ok(()) => {
            clear(&connection)?;
            Ok(changes.len())
        }
        Err(error) if landed_before(&error) => {
            clear(&connection)?;
            Ok(changes.len())
        }
        Err(error) if crate::store_remote::is_unreachable(&error) || unseeded(&error) => Err(error),
        Err(error) => {
            repair(&remote, &mut connection, &changes, last)?;
            Err(error)
        }
    }
}

/// 64-bit FNV-1a: a stable hash, the same in every build.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Take the primary's rows back for every row a refused batch touched, and
/// clear the batch from the outbox, in one local transaction. The primary
/// reads run first, outside the transaction.
fn repair(
    remote: &Remote,
    connection: &mut Connection,
    changes: &[Change],
    last: i64,
) -> Result<(), String> {
    let mut targets: Vec<(String, Vec<String>, Vec<String>, Vec<SqlValue>)> = Vec::new();
    for change in changes {
        let info = column_info(connection, &change.table)?;
        let mut key: Vec<(usize, i64)> = change
            .columns
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                info.iter()
                    .find(|(name, pk)| name == c && *pk > 0)
                    .map(|(_, pk)| (i, *pk))
            })
            .collect();
        key.sort_by_key(|(_, pk)| *pk);
        // A table with no declared key matches on every column.
        let key: Vec<usize> = if key.is_empty() {
            (0..change.columns.len()).collect()
        } else {
            key.into_iter().map(|(i, _)| i).collect()
        };
        let names: Vec<String> = key.iter().map(|&i| change.columns[i].clone()).collect();
        for row in [&change.old, &change.new] {
            if row.is_empty() {
                continue;
            }
            let values: Vec<SqlValue> = key.iter().map(|&i| row[i].clone()).collect();
            let target = (
                change.table.clone(),
                change.columns.clone(),
                names.clone(),
                values,
            );
            if !targets.contains(&target) {
                targets.push(target);
            }
        }
    }
    let mut fetched = Vec::new();
    for (table, columns, names, values) in &targets {
        let reply = remote.execute(
            &format!(
                "SELECT {} FROM \"{table}\" WHERE {}",
                quoted(columns),
                match_all(names, 1)
            ),
            values,
        )?;
        fetched.push(reply.rows);
    }
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| e.to_string())?;
    let mut nodes_moved = false;
    for ((table, columns, names, values), rows) in targets.iter().zip(fetched) {
        transaction
            .execute(
                &format!("DELETE FROM \"{table}\" WHERE {}", match_all(names, 1)),
                rusqlite::params_from_iter(values),
            )
            .map_err(|e| format!("{table}: {e}"))?;
        let insert = insert_sql(table, columns).replacen("INSERT", "INSERT OR REPLACE", 1);
        for row in &rows {
            transaction
                .execute(&insert, rusqlite::params_from_iter(row))
                .map_err(|e| format!("{table}: {e}"))?;
        }
        nodes_moved |= table == "nodes";
    }
    transaction
        .execute_batch(&format!(
            "DELETE FROM backlog_outbox WHERE id <= {last};
             DELETE FROM graph_meta WHERE key = '{SENDING}';"
        ))
        .map_err(|e| e.to_string())?;
    if nodes_moved {
        rebuild_search(&transaction)?;
    }
    stamp_fresh(&transaction, "repair")?;
    transaction.commit().map_err(|e| e.to_string())
}

/// After a write through `mutate_rows`, once the graph lock is gone: flush
/// the outbox. Only a refusal took the write back, so only a refusal syncs
/// the replica and returns, and the caller retries on fresh rows. Every
/// other failure leaves the write in the outbox for the next flush, and a
/// retry would apply it twice.
pub(crate) fn publish_after_write(graph: &Path) -> Result<(), String> {
    match flush(graph) {
        Ok(_) => Ok(()),
        Err(error)
            if error.starts_with(REFUSED)
                && !crate::store_remote::is_unreachable(&error)
                && !unseeded(&error) =>
        {
            if let Err(sync_error) = sync(graph) {
                eprintln!("backlog-share: sync after a refusal: {sync_error}");
            }
            Err(error)
        }
        Err(error) => {
            eprintln!("{error} The write is kept and sent on the next flush.");
            Ok(())
        }
    }
}

/// What one sync did.
#[derive(Debug, Default, PartialEq, serde::Serialize)]
pub struct Receipt {
    pub shared: bool,
    pub snapshot: bool,
    pub applied: usize,
    pub cursor: i64,
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

/// Apply the change log past `cursor`, one page per local transaction. Each
/// page is read from the primary before its transaction opens.
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
        let mut batches = Vec::new();
        for row in &page.rows {
            let seq = row[0].integer().ok_or("change row without a seq")?;
            batches.push((seq, parse_ops(row[1].text().unwrap_or("[]"))?));
        }
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        let mut nodes_moved = false;
        for (seq, changes) in &batches {
            for change in changes {
                if apply(&transaction, change)? {
                    receipt.applied += 1;
                    nodes_moved |= change.table == "nodes";
                }
            }
            cursor = *seq;
        }
        crate::backlog::stamp_meta(&transaction, CURSOR, &cursor.to_string())?;
        if nodes_moved {
            rebuild_search(&transaction)?;
        }
        stamp_fresh(&transaction, &cursor.to_string())?;
        transaction.commit().map_err(|e| e.to_string())?;
        receipt.cursor = cursor;
    }
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

/// Apply one logged change. The primary already decided it, and the log is
/// in order, so the replica takes the new row whatever it held: the old row
/// goes, and the new one replaces any row it collides with. `false` means
/// the replica already held the change, so a replay is harmless.
fn apply(connection: &Connection, change: &Change) -> Result<bool, String> {
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
        Op::Insert | Op::Update if present(&change.new)? => return Ok(false),
        Op::Delete if !present(&change.old)? => return Ok(false),
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
    Ok(true)
}

/// Triggers were off, so the search index rebuilds from the nodes it mirrors.
fn rebuild_search(connection: &Connection) -> Result<(), String> {
    crate::backlog::search::rebuild(connection)
}

/// A fresh content version, so a locked mutate that read before this write
/// sees a conflict and re-reads.
fn stamp_fresh(connection: &Connection, why: &str) -> Result<(), String> {
    crate::backlog::stamp_version(
        connection,
        &format!("sqlite:share-{why}-{}", crate::backlog::now_ms()),
    )
}

/// Every shared table's rows on the primary, read page by page.
fn primary_rows(
    remote: &Remote,
    columns: &BTreeMap<String, Vec<String>>,
) -> Result<Vec<(String, Vec<Vec<SqlValue>>)>, String> {
    let mut out = Vec::new();
    for (table, columns) in columns {
        let mut rows = Vec::new();
        let mut after = 0;
        loop {
            let page = remote.execute(
                &format!(
                    "SELECT rowid, {} FROM \"{table}\" WHERE rowid > ?1 ORDER BY rowid LIMIT ?2",
                    quoted(columns)
                ),
                &[SqlValue::Integer(after), SqlValue::Integer(PAGE)],
            )?;
            let full = page.rows.len() as i64 == PAGE;
            for mut row in page.rows {
                after = row[0].integer().unwrap_or(after);
                row.remove(0);
                rows.push(row);
            }
            if !full {
                break;
            }
        }
        out.push((table.clone(), rows));
    }
    Ok(out)
}

/// Replace every shared table with the primary's rows. All primary reads run
/// before the local transaction opens. The first snapshot keeps a backup
/// copy of the replica beside it, because this machine's own backlog rows
/// are replaced.
fn snapshot(remote: &Remote, graph: &Path, connection: &mut Connection) -> Result<Receipt, String> {
    let top = remote
        .execute("SELECT COALESCE(MAX(seq), 0) FROM backlog_changes", &[])?
        .rows
        .first()
        .and_then(|r| r[0].integer())
        .unwrap_or(0);
    let columns = shared_columns(connection)?;
    let tables = primary_rows(remote, &columns)?;
    if crate::backlog::meta(connection, CURSOR)?.is_none() {
        let db = crate::backlog::database_path(graph);
        let backup = db.with_extension(format!("pre-share-{}.db", crate::backlog::now_ms()));
        let mut copy = Connection::open(&backup).map_err(|e| e.to_string())?;
        rusqlite::backup::Backup::new(connection, &mut copy)
            .and_then(|b| b.run_to_completion(256, Duration::ZERO, None))
            .map_err(|e| format!("backup {}: {e}", backup.display()))?;
    }
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| e.to_string())?;
    let mut applied = 0;
    for (table, rows) in &tables {
        transaction
            .execute(&format!("DELETE FROM \"{table}\""), [])
            .map_err(|e| format!("{table}: {e}"))?;
        let insert = insert_sql(table, &columns[table]);
        for row in rows {
            transaction
                .execute(&insert, rusqlite::params_from_iter(row))
                .map_err(|e| format!("{table}: {e}"))?;
            applied += 1;
        }
    }
    crate::backlog::stamp_meta(&transaction, CURSOR, &top.to_string())?;
    guard(&transaction, &columns)?;
    rebuild_search(&transaction)?;
    stamp_fresh(&transaction, "snapshot")?;
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
    let db = crate::backlog::database_path(graph);
    let _lock = crate::graph_store::BoundedLock::acquire(&db.with_extension("share"), FLUSH_WAIT)
        .map_err(|e| e.to_string())?;
    let connection = replica(graph)?;
    // A write after this point may miss the copy, so its record stays.
    let carried: i64 = connection
        .query_row("SELECT COALESCE(MAX(id), 0) FROM backlog_outbox", [], |r| {
            r.get(0)
        })
        .map_err(|e| e.to_string())?;
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
                let sql = sql
                    .map_err(|e| e.to_string())?
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
    for table in SHARED_TABLES {
        let Some(columns) = columns.get(*table) else {
            continue;
        };
        let insert = insert_sql(table, columns);
        let mut steps = vec![(format!("DELETE FROM \"{table}\""), Vec::new())];
        let mut statement = connection
            .prepare(&format!("SELECT {} FROM \"{table}\"", quoted(columns)))
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
        counts.insert(table.to_string(), json!(count));
    }
    remote.script(
        "CREATE TABLE backlog_seed (at INTEGER NOT NULL); INSERT INTO backlog_seed (at) VALUES (0);",
    )?;
    let top = remote
        .execute("SELECT COALESCE(MAX(seq), 0) FROM backlog_changes", &[])?
        .rows
        .first()
        .and_then(|r| r[0].integer())
        .unwrap_or(0);
    crate::backlog::stamp_meta(&connection, CURSOR, &top.to_string())?;
    guard(&connection, &columns)?;
    // The seed carried every row these records name. A later record is sent
    // by the next flush; a primary that already holds its row refuses it,
    // and the repair takes back the row the seed copied.
    connection
        .execute_batch(&format!(
            "DELETE FROM backlog_outbox WHERE id <= {carried};
             DELETE FROM graph_meta WHERE key = '{SENDING}';"
        ))
        .map_err(|e| e.to_string())?;
    Ok(json!({"primary": remote.url(), "rows": counts, "cursor": top}))
}

/// `fno agents claim backlog seed|sync`.
pub fn run(args: &[String]) -> i32 {
    let graph = crate::backlog::settings::graph_path();
    let outcome = match args.first().map(String::as_str) {
        Some("seed") => seed(&graph),
        Some("sync") => flush(&graph).and_then(|sent| {
            let receipt = sync(&graph)?;
            Ok(json!({"sent": sent, "sync": receipt}))
        }),
        _ => {
            eprintln!(
                "usage: fno agents claim backlog seed|sync\n  seed  copy this machine's backlog into an empty shared primary\n  sync  send this machine's outbox, then bring its replica up to the primary"
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

/// The resident syncer: one flush and one sync every [`INTERVAL`], one in
/// flight, off the daemon's own loop.
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
        let graph = crate::backlog::settings::graph_path();
        if let Err(error) = flush(&graph) {
            eprintln!("backlog-share: flush: {error}");
        }
        if let Err(error) = sync(&graph) {
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

    /// The fixtures write rows the way any writer does, through the open
    /// seam; the store module owns the table, so its name rides a constant.
    const NODES: &str = "nodes";

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

    fn set_title(connection: &Connection, id: &str, title: &str) {
        connection
            .execute(
                &format!("UPDATE {NODES} SET title = ?2 WHERE id = ?1"),
                params![id, title],
            )
            .unwrap();
    }

    fn retitle(m: &Machine, id: &str, title: &str) {
        set_title(&crate::backlog::open(&m.graph).unwrap(), id, title);
    }

    fn outbox(m: &Machine) -> i64 {
        crate::backlog::open(&m.graph)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM backlog_outbox", [], |r| r.get(0))
            .unwrap()
    }

    fn add_node(m: &Machine, id: &str, title: &str) {
        let mut connection = crate::backlog::open(&m.graph).unwrap();
        let transaction = connection.transaction().unwrap();
        transaction
            .execute(
                &format!(
                    "INSERT INTO {NODES} (id, ordinal, slug, title, status, priority)
                     VALUES (?1, 1, ?1, 'draft', 'idea', 'p2')"
                ),
                params![id],
            )
            .unwrap();
        set_title(&transaction, id, title);
        transaction.commit().unwrap();
    }

    #[test]
    fn every_graph_table_is_shared_or_local() {
        let primary = test_primary::start();
        let m = machine();
        on(&primary, &m);
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
        let late_parents: Vec<(String, String)> = SHARED_TABLES
            .iter()
            .flat_map(|child| {
                let mut parents = connection
                    .prepare(&format!(
                        "SELECT \"table\" FROM pragma_foreign_key_list('{child}')"
                    ))
                    .unwrap();
                let late = parents
                    .query_map([], |r| r.get::<_, String>(0))
                    .unwrap()
                    .map(Result::unwrap)
                    .filter(|parent| parent != child && rank(parent) > rank(child))
                    .map(|parent| (child.to_string(), parent))
                    .collect::<Vec<_>>();
                late
            })
            .collect();
        route_to_primary(None);
        assert!(
            unclassified.is_empty(),
            "classify these tables: {unclassified:?}"
        );
        assert!(
            late_parents.is_empty(),
            "move each parent before its child in SHARED_TABLES: {late_parents:?}"
        );
    }

    #[test]
    fn a_flushed_write_lands_on_the_primary_and_a_peer_syncs_it() {
        let primary = test_primary::start();
        let (a, b) = (machine(), machine());
        on(&primary, &a);
        seed(&a.graph).unwrap();
        add_node(&a, "x-1", "shared");
        assert_eq!(
            flush(&a.graph).unwrap(),
            1,
            "insert then update fold to one row"
        );
        assert_eq!(outbox(&a), 0);
        let (on_primary, log): (String, i64) = primary
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT (SELECT title FROM nodes WHERE id = 'x-1'), (SELECT COUNT(*) FROM backlog_changes)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((on_primary.as_str(), log), ("shared", 1));

        on(&primary, &b);
        assert!(sync(&b.graph).unwrap().snapshot);
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

        // A build without this module registers no writer function, so the
        // guard refuses its write on both the seeding and the synced store.
        for m in [&a, &b] {
            let old_build = Connection::open(crate::backlog::database_path(&m.graph)).unwrap();
            let refused = old_build
                .execute(
                    &format!("UPDATE {NODES} SET title = 'old build' WHERE id = 'x-1'"),
                    [],
                )
                .unwrap_err()
                .to_string();
            assert!(refused.contains(WRITER), "{refused}");
        }
        assert_eq!(title(&a, "x-1").as_deref(), Some("shared"));

        // Sharing off: the next open drops the guard, so an older build
        // writes again.
        route_to_primary(None);
        drop(crate::backlog::open(&a.graph).unwrap());
        Connection::open(crate::backlog::database_path(&a.graph))
            .unwrap()
            .execute(
                &format!("UPDATE {NODES} SET title = 'old build' WHERE id = 'x-1'"),
                [],
            )
            .unwrap();
    }

    #[test]
    fn a_stale_write_is_refused_and_an_unreachable_one_waits() {
        let primary = test_primary::start();
        let (a, b) = (machine(), machine());
        on(&primary, &a);
        seed(&a.graph).unwrap();
        add_node(&a, "x-1", "first");
        flush(&a.graph).unwrap();
        on(&primary, &b);
        sync(&b.graph).unwrap();
        on(&primary, &a);
        retitle(&a, "x-1", "from a");
        flush(&a.graph).unwrap();

        on(&primary, &b);
        retitle(&b, "x-1", "from b");
        let refusal = flush(&b.graph).unwrap_err();
        assert!(
            refusal.starts_with(REFUSED) && refusal.contains("nodes row changed"),
            "{refusal}"
        );
        assert_eq!(title(&b, "x-1").as_deref(), Some("from a"));
        assert_eq!(outbox(&b), 0);

        // A second seed refuses; an unreachable primary keeps the outbox.
        on(&primary, &a);
        assert!(seed(&a.graph).unwrap_err().contains("already holds"));
        route_to_primary(Some((
            test_primary::dead(),
            crate::backlog::database_path(&a.graph),
        )));
        add_node(&a, "x-2", "waits");
        assert!(flush(&a.graph).unwrap_err().contains("is unreachable"));
        assert!(outbox(&a) > 0);
        route_to_primary(None);
    }
}
