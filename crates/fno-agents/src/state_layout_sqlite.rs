//! The SQLite move protocol (the layout table's `sqlite` kind): gate, fence,
//! drain, copy, publish, park, verify. Nothing is deleted. A refused row
//! keeps both copies; a pending row retries on the next lane pass.

use crate::state_layout::{find, Row, Status};
use rusqlite::Connection;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The fence file a moving store writes under `db/`: every new-build opener
/// of these stores waits for it to clear (bounded) before it opens, then
/// resolves again through `place`.
pub const FENCE_NAME: &str = ".migrating";

const FENCE_WAIT: Duration = Duration::from_secs(30);
const DRAIN_BUSY_TIMEOUT_MS: u64 = 5_000;
/// The verify pass runs on the NEXT lane pass; stamps younger than this are
/// left for the pass after.
const VERIFY_MIN_AGE: Duration = Duration::from_secs(10);

pub fn fence_path(root: &Path) -> PathBuf {
    root.join("db").join(FENCE_NAME)
}

/// The fence names its writer's pid; only that writer removes it. A
/// concurrent pass that lost the write race must never strip the winner's
/// fence out from under a move still running.
fn clear_own_fence(fence: &Path, pid: u32) {
    let owns = std::fs::read_to_string(fence)
        .ok()
        .and_then(|t| t.trim().parse::<u32>().ok())
        == Some(pid);
    if owns {
        let _ = std::fs::remove_file(fence);
    }
}

/// Block until the fence clears. A fence naming a dead pid (or an unreadable
/// one past a bounded grace) is a crashed run's residue and clears here. A
/// live fence holds the caller at most [`FENCE_WAIT`]; after that the opener
/// proceeds and the next `place` read decides what exists.
pub fn wait_for_fence(root: &Path) {
    let fence = fence_path(root);
    let deadline = std::time::Instant::now() + FENCE_WAIT;
    loop {
        match std::fs::read_to_string(&fence) {
            Err(_) => return,
            Ok(text) => {
                let pid_alive = text.trim().parse::<u32>().map(is_pid_alive).unwrap_or(true);
                if !pid_alive {
                    let _ = std::fs::remove_file(&fence);
                    return;
                }
            }
        }
        if std::time::Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn is_pid_alive(pid: u32) -> bool {
    // kill 0 probes liveness without signalling; EPERM means the process
    // exists but belongs to another user, which is still alive.
    unsafe {
        libc::kill(pid as i32, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

/// The census gate: a live store-keeper older than the running build would
/// keep serving the parked inode after the move, so its pid holds the row
/// pending and the lane retries next pass. A census that could not run (no
/// rows at all) is dark: pending, never a pass.
fn stale_keeper_pid(graph_db: &Path) -> Result<Option<u32>, String> {
    let rows = crate::census::census_blocking();
    if rows.is_empty() {
        return Err("census unavailable".to_string());
    }
    let want = same_file_key(graph_db);
    for r in &rows {
        if r.get("component").and_then(|v| v.as_str()) != Some("store-keeper") {
            continue;
        }
        if r.get("verdict").and_then(|v| v.as_str()) != Some("stale") {
            continue;
        }
        let Some(graph) = r.get("graph").and_then(|v| v.as_str()) else {
            continue;
        };
        if same_file_key(Path::new(graph)) == want {
            return Ok(r.get("pid").and_then(|v| v.as_u64()).map(|p| p as u32));
        }
    }
    Ok(None)
}

fn same_file_key(p: &Path) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    match std::fs::metadata(p) {
        Ok(m) => (m.dev(), m.ino()),
        Err(_) => (0, 0),
    }
}

/// Per-table row counts: the copy check and the verify record.
fn table_counts(conn: &Connection) -> Result<BTreeMap<String, i64>, String> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")
        .map_err(|e| e.to_string())?;
    let names: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .map_err(|e| e.to_string())?
        .filter_map(|r| r.ok())
        .collect();
    let mut out = BTreeMap::new();
    for name in names {
        // sqlite_master names are store-owned identifiers; an embedded
        // double quote must be doubled or the COUNT reads as new SQL.
        let quoted = name.replace('"', "\"\"");
        let sql = format!("SELECT COUNT(*) FROM \"{quoted}\"");
        let n: i64 = conn
            .query_row(&sql, [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        out.insert(name, n);
    }
    Ok(out)
}

/// The verify record: per-table row counts plus the high-water `seq` where a
/// table carries one. A stale writer's UPDATE-in-place bumps `seq` without
/// changing row counts; the high-water catches it.
fn write_verify_record(
    dir: &Path,
    counts: &BTreeMap<String, i64>,
    seqs: &BTreeMap<String, i64>,
) -> Result<(), String> {
    let mut text = String::new();
    for (name, n) in counts {
        let s = seqs
            .get(name)
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".to_string());
        text.push_str(&format!("{name}\t{n}\t{s}\n"));
    }
    std::fs::write(dir.join("verify.tsv"), text).map_err(|e| e.to_string())
}

/// The per-table high-water `seq` for every user table that carries the
/// column. Absent where it does not.
fn seq_high_water(
    conn: &Connection,
    counts: &BTreeMap<String, i64>,
) -> Result<BTreeMap<String, i64>, String> {
    let mut out = BTreeMap::new();
    for name in counts.keys() {
        let lit = name.replace('\'', "''");
        let has_seq: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM pragma_table_info('{lit}') WHERE name = 'seq'"),
                [],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        if has_seq == 0 {
            continue;
        }
        let quoted = name.replace('"', "\"\"");
        let m: Option<i64> = conn
            .query_row(&format!("SELECT MAX(seq) FROM \"{quoted}\""), [], |r| {
                r.get(0)
            })
            .map_err(|e| e.to_string())?;
        if let Some(m) = m {
            out.insert(name.clone(), m);
        }
    }
    Ok(out)
}

/// The 10s-later verify: the parked copy must still hold the row counts
/// recorded at park time. A diverged parked file means an old-build writer
/// kept serving the parked inode after the move; the row reads refused and
/// both copies stay for a human. Nothing is deleted on a mismatch; a clean
/// verify removes only the record.
fn verify_stamp(backup_root: &Path, stamp: &str) -> Option<Status> {
    let dir = backup_root.join(stamp);
    let record_path = dir.join("verify.tsv");
    let record = std::fs::read_to_string(&record_path).ok()?;
    let age_ok = std::fs::metadata(&record_path)
        .and_then(|m| m.modified())
        .ok()?
        .elapsed()
        .map(|age| age >= VERIFY_MIN_AGE)
        .unwrap_or(false);
    if !age_ok {
        return None;
    }
    // Two-field lines predate the seq high-water; their counts still verify.
    let mut then_counts: BTreeMap<String, i64> = BTreeMap::new();
    let mut then_seqs: BTreeMap<String, i64> = BTreeMap::new();
    for line in record.lines() {
        let mut parts = line.split('\t');
        let (Some(name), Some(count)) = (parts.next(), parts.next()) else {
            return None;
        };
        let (Some(name), Ok(count)) = (Some(name.to_string()), count.parse::<i64>()) else {
            return None;
        };
        then_counts.insert(name.clone(), count);
        if let Some(seq) = parts.next() {
            if let Ok(seq) = seq.parse::<i64>() {
                then_seqs.insert(name, seq);
            }
        }
    }
    let parked_db = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "db"))?;
    let conn =
        match Connection::open_with_flags(&parked_db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(c) => c,
            Err(_) => {
                return Some(Status::Refused(
                    "parked copy changed after publish".to_string(),
                ))
            }
        };
    match table_counts(&conn) {
        Ok(now) if now == then_counts => {
            let seqs_ok = match seq_high_water(&conn, &now) {
                Ok(now_seqs) => then_seqs
                    .iter()
                    .all(|(name, seq)| now_seqs.get(name) == Some(seq)),
                Err(_) => false,
            };
            if seqs_ok {
                let _ = std::fs::remove_file(&record_path);
                return Some(Status::Moved);
            }
            Some(Status::Refused(
                "parked copy changed after publish".to_string(),
            ))
        }
        Ok(_) | Err(_) => Some(Status::Refused(
            "parked copy changed after publish".to_string(),
        )),
    }
}

/// True when the given legacy name is one of the -wal/-shm sidecars that
/// travel with a base .db row (they never run the protocol themselves).
fn is_sidecar(legacy: &str) -> bool {
    legacy.ends_with("-wal") || legacy.ends_with("-shm")
}

/// The protocol for one `sqlite` row. Only base `.db` names run it; the
/// -wal/-shm rows report through their base row. Called from the layout
/// migration for every `kind = sqlite` row.
pub fn migrate_sqlite_row(root: &Path, row: &Row, apply: bool, stamp: &str) -> Status {
    if is_sidecar(&row.legacy) {
        return sidecar_status(root, row);
    }
    let legacy = root.join(&row.legacy);
    if !legacy.exists() {
        return match root.join(&row.new).exists() {
            true => Status::Moved,
            false => Status::Absent,
        };
    }
    if !apply {
        return Status::Pending("sqlite move (dry run)".to_string());
    }
    // Gate: a stale keeper keeps serving the parked inode after the move.
    match stale_keeper_pid(&legacy) {
        Err(dark) => return Status::Pending(dark),
        Ok(Some(pid)) => return Status::Pending(format!("stale keeper pid {pid}")),
        Ok(None) => {}
    }
    let new = root.join(&row.new);
    if new.exists() {
        return both_exist(root, row, &legacy, &new, stamp);
    }
    run_protocol(root, row, &legacy, &new, stamp)
}

/// The -wal/-shm rows read through their base row's outcome.
fn sidecar_status(root: &Path, row: &Row) -> Status {
    let base_legacy = root.join(&row.legacy);
    if base_legacy.exists() {
        return Status::Pending("waits for its base row".to_string());
    }
    let base_name = row
        .legacy
        .strip_suffix("-wal")
        .or_else(|| row.legacy.strip_suffix("-shm"))
        .unwrap_or(&row.legacy);
    match find(base_name) {
        Some(base) => {
            if root.join(&base.new).exists() {
                Status::Moved
            } else {
                Status::Absent
            }
        }
        None => Status::Absent,
    }
}

/// The sweep over earlier stamps: every verify.tsv older than the verify
/// delay gets its parked copy compared against its recorded counts. Entries
/// land on the receipt naming the stamp.
pub fn verify_sweep(root: &Path, stamp: &str) -> Vec<(String, Status)> {
    let backup_root = root.join("backups").join("state-root-migration");
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&backup_root) else {
        return out;
    };
    for e in entries.filter_map(|e| e.ok()) {
        let name = e.file_name().to_string_lossy().to_string();
        if name == stamp {
            continue;
        }
        if let Some(status) = verify_stamp(&backup_root, &name) {
            out.push((name, status));
        }
    }
    out
}

/// The both-exist rule: a fully empty new file is a race's fresh create;
/// park it and migrate the legacy file. Rows on both sides stay for a human.
fn both_exist(root: &Path, row: &Row, legacy: &Path, new: &Path, stamp: &str) -> Status {
    let new_empty =
        match Connection::open_with_flags(new, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(c) => empty_store(&c).unwrap_or(false),
            Err(_) => false,
        };
    if new_empty {
        let dir = backup_dir(root, stamp);
        if std::fs::create_dir_all(&dir).is_err() {
            return Status::Refused("cannot create the backup folder".to_string());
        }
        let parked_name = format!("{}.empty-at-publish", row.legacy);
        if std::fs::rename(new, dir.join(parked_name)).is_err() {
            return Status::Refused("cannot park the empty new file".to_string());
        }
        return run_protocol(root, row, legacy, new, stamp);
    }
    Status::Refused("both copies hold rows".to_string())
}

/// Empty means: no user tables, or every user table holds zero rows.
fn empty_store(conn: &Connection) -> Result<bool, String> {
    let counts = table_counts(conn)?;
    Ok(counts.values().all(|n| *n == 0))
}

fn backup_dir(root: &Path, stamp: &str) -> PathBuf {
    root.join("backups")
        .join("state-root-migration")
        .join(stamp)
}

/// One rollback helper so every failure arm reads the same: drop the drain,
/// clear the copy target, remove the fence. The drain transaction rolls
/// back; the legacy trio stays exactly as it was.
struct Drain {
    fence: PathBuf,
    migrating: PathBuf,
}

impl Drain {
    fn fail(self, guard: &Connection, pid: u32, why: String) -> Status {
        let _ = guard.execute_batch("ROLLBACK;");
        let _ = std::fs::remove_file(&self.migrating);
        clear_own_fence(&self.fence, pid);
        Status::Refused(why)
    }
}

/// The busy-retry ceiling for the backup: the copy waits this long for its
/// pages, then the row reads refused and nothing is deleted. A write
/// transaction open on the copy source turns every step Busy, and the stock
/// run_to_completion retries Busy forever - the ceiling exists so a future
/// lock shape can never hang the lane again.
const BACKUP_BUDGET: Duration = Duration::from_secs(30);

/// The drain-copy-publish-park body. The fence is written before the first
/// open and removed last. The drain transaction stays open until the legacy
/// trio has left, so a writer that arrives mid-move blocks on busy_timeout
/// instead of writing a file nobody will publish.
fn run_protocol(root: &Path, row: &Row, legacy: &Path, new: &Path, stamp: &str) -> Status {
    if std::fs::create_dir_all(root.join("db")).is_err() {
        return Status::Refused("cannot create db/".to_string());
    }
    let fence = fence_path(root);
    if let Ok(text) = std::fs::read_to_string(&fence) {
        let alive = text.trim().parse::<u32>().map(is_pid_alive).unwrap_or(true);
        if alive {
            return Status::Pending("migration fence live".to_string());
        }
        // A dead writer's residue: clear it so the create below can win.
        let _ = std::fs::remove_file(&fence);
    }
    let pid = std::process::id();
    // The fence is claimed atomically: create_new means exactly one writer
    // owns the move, the loser reads pending, and clear_own_fence can never
    // strip the winner's fence.
    let claimed = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&fence)
        .and_then(|mut f| {
            use std::io::Write;
            f.write_all(pid.to_string().as_bytes())
        });
    match claimed {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Status::Pending("migration fence live".to_string());
        }
        Err(_) => return Status::Refused("cannot write the fence".to_string()),
        Ok(()) => {}
    }
    // The drain transaction lives on its own guard connection: an open
    // write transaction on the copy SOURCE turns every backup step Busy
    // (sqlite reads its own uncommitted state as an unsnapshotted write),
    // so the guard holds RESERVED to park other writers while a second,
    // transaction-free connection takes the copy. Both opens refuse to
    // create: a concurrent pass that published first renames the legacy
    // file away, and a CREATE open here would materialize an empty store
    // in its place and publish that emptiness over the winner's copy.
    // READ_WRITE alone: CREATE is a separate default bit, so this open
    // refuses to materialize a missing store.
    let no_create = rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE;
    let guard = match Connection::open_with_flags(legacy, no_create) {
        Ok(c) => c,
        Err(e) => {
            clear_own_fence(&fence, pid);
            return Status::Refused(format!("cannot open the legacy store: {e}"));
        }
    };
    let _ = guard.busy_timeout(Duration::from_millis(DRAIN_BUSY_TIMEOUT_MS));
    if let Err(e) = guard.execute_batch("BEGIN IMMEDIATE") {
        clear_own_fence(&fence, pid);
        return Status::Pending(format!(
            "a writer holds the legacy store past the busy timeout ({e})"
        ));
    }
    let migrating = migrating_path(new);
    let drain = Drain {
        fence: fence.clone(),
        migrating: migrating.clone(),
    };
    // Copy through the SQLite backup API into db/<name>.migrating.
    let src = match Connection::open_with_flags(legacy, no_create) {
        Ok(c) => c,
        Err(e) => return drain.fail(&guard, pid, format!("cannot open the copy source: {e}")),
    };
    let _ = std::fs::remove_file(&migrating);
    let mut dst = match Connection::open(&migrating) {
        Ok(c) => c,
        Err(e) => return drain.fail(&guard, pid, format!("cannot create the copy target: {e}")),
    };
    let backup = rusqlite::backup::Backup::new(&src, &mut dst)
        .map_err(|e| e.to_string())
        .and_then(|b| {
            let deadline = std::time::Instant::now() + BACKUP_BUDGET;
            loop {
                match b.step(5) {
                    Ok(rusqlite::backup::StepResult::More) => {}
                    Ok(rusqlite::backup::StepResult::Done) => return Ok(()),
                    // Busy and Locked arrive as Ok variants; they retry to
                    // the ceiling like the Err arm, then refuse.
                    Ok(
                        rusqlite::backup::StepResult::Busy | rusqlite::backup::StepResult::Locked,
                    ) => {
                        if std::time::Instant::now() >= deadline {
                            return Err("backup stayed busy past the budget".to_string());
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Ok(other) => return Err(format!("backup step read {other:?}")),
                    Err(e) => {
                        if std::time::Instant::now() >= deadline {
                            return Err(format!("backup stayed busy past the budget: {e}"));
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            }
        });
    if let Err(e) = backup {
        return drain.fail(&guard, pid, format!("backup copy failed: {e}"));
    }
    let counts = match table_counts(&src) {
        Ok(c) => c,
        Err(e) => return drain.fail(&guard, pid, format!("cannot count the legacy store: {e}")),
    };
    let integrity: String = match dst.query_row("PRAGMA integrity_check;", [], |r| r.get(0)) {
        Ok(v) => v,
        Err(e) => return drain.fail(&guard, pid, format!("integrity check failed: {e}")),
    };
    if integrity != "ok" {
        return drain.fail(&guard, pid, format!("integrity_check read {integrity}"));
    }
    let dst_counts = match table_counts(&dst) {
        Ok(c) => c,
        Err(e) => return drain.fail(&guard, pid, format!("cannot count the copy: {e}")),
    };
    if counts != dst_counts {
        return drain.fail(
            &guard,
            pid,
            "row counts diverge between the copies".to_string(),
        );
    }
    // Publish: fsync the copy file, then rename it over db/<name>.
    if std::fs::File::open(&migrating)
        .and_then(|f| f.sync_all())
        .is_err()
    {
        return drain.fail(&guard, pid, "cannot fsync the copy".to_string());
    }
    drop(dst);
    if std::fs::rename(&migrating, new).is_err() {
        return drain.fail(&guard, pid, "cannot publish the copy".to_string());
    }
    // Park the legacy trio, record the verify counts, close the drain.
    let dir = backup_dir(root, stamp);
    if std::fs::create_dir_all(&dir).is_err() {
        return drain.fail(&guard, pid, "cannot create the backup folder".to_string());
    }
    for suffix in ["", "-wal", "-shm"] {
        let name = format!("{}{suffix}", row.legacy);
        let p = root.join(&name);
        if p.exists() && std::fs::rename(&p, dir.join(&name)).is_err() {
            return drain.fail(&guard, pid, format!("cannot park {name}"));
        }
    }
    let seqs = match seq_high_water(&src, &counts) {
        Ok(s) => s,
        Err(e) => return drain.fail(&guard, pid, format!("cannot read the seq high-water: {e}")),
    };
    let _ = write_verify_record(&dir, &counts, &seqs);
    let _ = guard.execute_batch("ROLLBACK;");
    drop(src);
    drop(guard);
    clear_own_fence(&fence, pid);
    Status::Moved
}

fn migrating_path(new: &Path) -> PathBuf {
    let name = new.file_name().and_then(|n| n.to_str()).unwrap_or("store");
    new.with_file_name(format!("{name}.migrating"))
}

/// The straggler lane: a stale long-lived writer (an old-build mux server)
/// can recreate a legacy `events.db`, `decisions.db` or `questions.db` after
/// the move published `db/<stem>.db`. One lane pass imports its rows into the
/// published store by `event_id` with INSERT OR IGNORE, then parks the file.
/// A refused row keeps both copies; the pass retries it next time.
pub fn straggler_import(root: &Path, stamp: &str, apply: bool) -> Vec<(String, Status)> {
    let mut out = Vec::new();
    if !apply {
        return out;
    }
    for stem in ["events", "decisions", "questions"] {
        let name = format!("{stem}.db");
        let legacy = root.join(&name);
        if !legacy.exists() {
            continue;
        }
        let new = root.join("db").join(&name);
        if !new.exists() {
            continue;
        }
        match import_straggler(&legacy, &new) {
            Ok(_) => {
                let dir = backup_dir(root, stamp);
                if std::fs::create_dir_all(&dir).is_ok()
                    && std::fs::rename(&legacy, dir.join(&name)).is_ok()
                {
                    out.push((name, Status::Merged));
                } else {
                    out.push((
                        name,
                        Status::Refused("cannot park the imported straggler".to_string()),
                    ));
                }
            }
            Err(e) => out.push((
                name,
                Status::Refused(format!("straggler import failed: {e}")),
            )),
        }
    }
    out
}

/// Import one straggler's events rows into the published store. The straggler
/// migrates to v2 in place first (a v1 file carries no `event_id` to import
/// by), then the v2 rows INSERT OR IGNORE on `seq` and `event_id` conflicts.
fn import_straggler(legacy: &Path, new: &Path) -> Result<usize, String> {
    let mut src = {
        let c = Connection::open_with_flags(legacy, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
            .map_err(|e| e.to_string())?;
        let _ = c.busy_timeout(Duration::from_secs(5));
        c
    };
    crate::event_store::ensure_schema(&mut src, legacy)?;
    let dst = Connection::open_with_flags(new, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(|e| e.to_string())?;
    let _ = dst.busy_timeout(Duration::from_secs(5));
    let columns: Vec<String> = {
        let mut stmt = dst
            .prepare("SELECT name FROM pragma_table_info('events') ORDER BY cid")
            .map_err(|e| e.to_string())?;
        let names: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        if names.is_empty() || !names.iter().any(|c| c == "event_id") {
            return Err("the published store has no v2 events table".to_string());
        }
        names
    };
    let src_columns: Vec<String> = {
        let mut stmt = src
            .prepare("SELECT name FROM pragma_table_info('events') ORDER BY cid")
            .map_err(|e| e.to_string())?;
        let names: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        names
    };
    if src_columns != columns {
        return Err("the straggler's events columns diverge from the published store".to_string());
    }
    let placeholders: Vec<&str> = columns.iter().map(|_| "?").collect();
    let insert = format!(
        "INSERT OR IGNORE INTO events ({}) VALUES ({})",
        columns.join(", "),
        placeholders.join(", ")
    );
    let select = format!("SELECT {} FROM events ORDER BY seq", columns.join(", "));
    let mut imported = 0usize;
    let tx = dst.unchecked_transaction().map_err(|e| e.to_string())?;
    {
        let mut rows = src.prepare(&select).map_err(|e| e.to_string())?;
        let mut rows = rows.query([]).map_err(|e| e.to_string())?;
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let as_params: Vec<Box<dyn rusqlite::types::ToSql>> = columns
                .iter()
                .map(|c| {
                    let v: rusqlite::types::Value =
                        row.get(c.as_str()).unwrap_or(rusqlite::types::Value::Null);
                    Box::new(v) as Box<dyn rusqlite::types::ToSql>
                })
                .collect();
            let params: Vec<&dyn rusqlite::types::ToSql> =
                as_params.iter().map(|b| b.as_ref()).collect();
            imported += dst
                .execute(&insert, params.as_slice())
                .map_err(|e| e.to_string())?;
        }
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(imported)
}
