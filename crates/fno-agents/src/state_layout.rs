//! The state-root layout: one table (`docs/state-root-layout.tsv`, read at
//! build time) that names where every movable top-level file of the state
//! root lives after the migration, plus the one resolver ([`place`]) and the
//! one mover ([`migrate`]). The tidiness law: nothing writes at the top level
//! of the state root when a named subfolder can hold it.
//!
//! `place` is read-old-as-fallback: a migrated root reads the new path, an
//! unmigrated root keeps reading the legacy one, a fresh install gets the
//! new path. Nothing here ever deletes data: the only deletion is an empty
//! legacy lock file after its data file moved and a non-blocking flock
//! succeeds, and every conflict parks into `backups/state-root-migration/`.
//! The `sqlite` kind moves through a backup-API protocol in a later wave;
//! until that lands the migrate pass reports those rows pending.
//!
//! The `fno` crate carries its own copy of the table parse and `place`
//! (`crates/fno/src/state_layout.rs`), same file, same dialect - the
//! dual-implementation inventory pattern (one TSV, two readers, no third
//! mechanism).

use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The shipped layout table, vendored beside this module: a `cargo package`
/// tarball carries only crate-root files, so the repo's `docs/` copy cannot
/// feed `include_str!` in a packaged build (the publish dry-run proved it).
/// The repo file is the one EDIT source; the drift test pins this copy
/// byte-identical to it, so a table edit that skips the copies fails CI.
pub const LAYOUT_TSV: &str = include_str!("state-root-layout.tsv");

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Sqlite,
    Anchor,
    Append,
    Overwrite,
    Marker,
    Page,
    Lock,
    Park,
}

impl Kind {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "sqlite" => Self::Sqlite,
            "anchor" => Self::Anchor,
            "append" => Self::Append,
            "overwrite" => Self::Overwrite,
            "marker" => Self::Marker,
            "page" => Self::Page,
            "lock" => Self::Lock,
            "park" => Self::Park,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Owner {
    Daemon,
    Mux,
}

impl Owner {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "daemon" => Self::Daemon,
            "mux" => Self::Mux,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug)]
pub struct Row {
    pub legacy: String,
    pub new: String,
    pub kind: Kind,
    pub owner: Owner,
}

/// Parse the table. Every failure names its 1-based line so a bad row can
/// never ship silently (the unit tests pin both failure shapes).
pub fn parse_table(tsv: &str) -> Result<Vec<Row>, String> {
    let mut out = Vec::new();
    for (idx, line) in tsv.lines().enumerate() {
        let no = idx + 1;
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        let bad = |why: &str| -> String {
            format!("state-root layout table line {no}: {why} ({line:?})")
        };
        let [legacy, new, kind, owner] = fields.as_slice() else {
            return Err(bad("expected 4 TAB-separated fields"));
        };
        if legacy.is_empty() || new.is_empty() {
            return Err(bad("empty name"));
        }
        let Some(kind) = Kind::parse(kind) else {
            return Err(bad("unknown kind"));
        };
        let Some(owner) = Owner::parse(owner) else {
            return Err(bad("unknown owner"));
        };
        out.push(Row {
            legacy: legacy.to_string(),
            new: new.to_string(),
            kind,
            owner,
        });
    }
    if out.is_empty() {
        return Err("state-root layout table: no rows".to_string());
    }
    Ok(out)
}

/// The shipped rows, parsed once. A malformed shipped table panics loud
/// rather than resolving guessed paths; the unit tests keep that unreachable.
pub fn rows() -> &'static [Row] {
    static ROWS: OnceLock<&'static [Row]> = OnceLock::new();
    *ROWS.get_or_init(|| {
        let parsed = parse_table(LAYOUT_TSV).unwrap_or_else(|e| panic!("{e}"));
        Box::leak(parsed.into_boxed_slice())
    })
}

pub fn find(legacy_name: &str) -> Option<&'static Row> {
    rows().iter().find(|r| r.legacy == legacy_name)
}

/// The anchor probe: a `graph.json`-shaped path exists when its `.db`
/// sibling does - the journal anchor is virtual, the store is the file.
fn db_twin(p: &Path) -> PathBuf {
    p.with_extension("db")
}

/// Where `legacy_name` lives under `root`: the new path when it exists, else
/// the legacy path when it exists, else the new path (fresh install). The
/// `anchor` kind probes the `.db` twin instead of the virtual json. A name
/// the table does not carry resolves to `root/<name>` unchanged.
pub fn place(root: &Path, legacy_name: &str) -> PathBuf {
    let Some(row) = find(legacy_name) else {
        return root.join(legacy_name);
    };
    let new = root.join(&row.new);
    let legacy = root.join(&row.legacy);
    match row.kind {
        Kind::Anchor => {
            if db_twin(&new).exists() {
                new
            } else if db_twin(&legacy).exists() {
                legacy
            } else {
                new
            }
        }
        _ => {
            if new.exists() {
                new
            } else if legacy.exists() {
                legacy
            } else {
                new
            }
        }
    }
}

// ── The migration ────────────────────────────────────────────────────────

/// One entry's outcome. `absent` = nothing at the legacy path and the new
/// path already in place or equally empty: no work now, none owed, and it
/// does not count toward the pending total. `deleted` = the one lawful
/// deletion (an emptied lock). Everything else is the plan's vocabulary.
#[derive(Debug, PartialEq, Eq)]
pub enum Status {
    Moved,
    Merged,
    Parked,
    Deleted,
    Absent,
    Pending(String),
    Refused(String),
}

impl Status {
    fn word(&self) -> &'static str {
        match self {
            Status::Moved => "moved",
            Status::Merged => "merged",
            Status::Parked => "parked",
            Status::Deleted => "deleted",
            Status::Absent => "absent",
            Status::Pending(_) => "pending",
            Status::Refused(_) => "refused",
        }
    }

    fn detail(&self) -> Option<&str> {
        match self {
            Status::Pending(d) | Status::Refused(d) => Some(d),
            _ => None,
        }
    }

    fn is_pending(&self) -> bool {
        matches!(self, Status::Pending(_))
    }
}

#[derive(Debug)]
pub struct Entry {
    pub legacy: String,
    pub status: Status,
}

#[derive(Debug, Default)]
pub struct Receipt {
    pub entries: Vec<Entry>,
}

impl Receipt {
    /// Entries still owed work: a dry run of movable rows, a later-wave kind,
    /// a mux-owned row waiting for its server, a lock waiting for its data
    /// file. The close probe reads this: `.pending == 0`.
    pub fn pending_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.status.is_pending())
            .count()
    }

    pub fn refused_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| matches!(e.status, Status::Refused(_)))
            .count()
    }

    fn count_matching(&self, pred: fn(&Status) -> bool) -> usize {
        self.entries.iter().filter(|e| pred(&e.status)).count()
    }

    pub fn summary(&self) -> String {
        format!(
            "{} moved, {} merged, {} parked, {} deleted, {} pending, {} refused, {} absent",
            self.count_matching(|s| matches!(s, Status::Moved)),
            self.count_matching(|s| matches!(s, Status::Merged)),
            self.count_matching(|s| matches!(s, Status::Parked)),
            self.count_matching(|s| matches!(s, Status::Deleted)),
            self.pending_count(),
            self.refused_count(),
            self.count_matching(|s| matches!(s, Status::Absent)),
        )
    }
}

/// One migration pass over the whole table against one state root.
/// Dry run (default) reports every owed row `pending`; `apply` moves,
/// merges, parks and deletes per kind. Idempotent: a second apply over a
/// migrated root reports moved/absent and parks nothing new.
pub fn migrate(root: &Path, apply: bool) -> Receipt {
    let stamp = utc_stamp();
    let mut receipt = Receipt::default();
    // The sqlite family's deferred verify: stamps parked on earlier passes
    // get their parked copies compared against their recorded counts here.
    for (name, status) in crate::state_layout_sqlite::verify_sweep(root, &stamp) {
        receipt.entries.push(Entry {
            legacy: format!("{name} (verify)"),
            status,
        });
    }
    for row in rows() {
        match row.kind {
            // Glob rows: every root entry the pattern matches parks.
            Kind::Park if row.legacy.contains('*') => {
                let names = glob_matches(root, &row.legacy);
                if names.is_empty() {
                    continue;
                }
                // The mux server owns its residue's timing (change 3.1); the
                // daemon never parks mux rows, glob or not.
                if row.owner == Owner::Mux {
                    receipt.entries.push(Entry {
                        legacy: row.legacy.to_string(),
                        status: Status::Pending(
                            "owner mux (the mux server parks it at its start)".to_string(),
                        ),
                    });
                    continue;
                }
                for name in names {
                    let status = if apply {
                        park_entry(root, &stamp, &name)
                    } else {
                        Status::Pending("park (dry run)".to_string())
                    };
                    receipt.entries.push(Entry {
                        legacy: name,
                        status,
                    });
                }
            }
            Kind::Park => {
                let status = if !root.join(&row.legacy).exists() {
                    Status::Absent
                } else if apply {
                    park_entry(root, &stamp, &row.legacy)
                } else {
                    Status::Pending("park (dry run)".to_string())
                };
                receipt.entries.push(Entry {
                    legacy: row.legacy.to_string(),
                    status,
                });
            }
            _ => {
                let status = migrate_entry(root, row, apply, &stamp);
                receipt.entries.push(Entry {
                    legacy: row.legacy.to_string(),
                    status,
                });
            }
        }
    }
    receipt
}

fn migrate_entry(root: &Path, row: &Row, apply: bool, stamp: &str) -> Status {
    if row.kind == Kind::Lock {
        return migrate_lock(root, row, apply, stamp);
    }
    let legacy = root.join(&row.legacy);
    let new = root.join(&row.new);
    let legacy_exists = legacy.exists();
    let new_exists = new.exists();
    if !legacy_exists && !new_exists {
        return Status::Absent;
    }
    // Already migrated: the new path holds the file, nothing at the legacy
    // path owes a move.
    if !legacy_exists && new_exists {
        return Status::Moved;
    }
    // Skipped families. Their legacy file EXISTS here, so the work is real
    // and reads pending: the mux server moves its own rows at its start
    // (change 3.1).
    if row.owner == Owner::Mux {
        return Status::Pending("owner mux (the mux server moves it at its start)".to_string());
    }
    if row.kind == Kind::Sqlite {
        return crate::state_layout_sqlite::migrate_sqlite_row(root, row, apply, stamp);
    }
    if !apply {
        return Status::Pending(match row.kind {
            Kind::Append if legacy_exists && new_exists => "append merge (dry run)".to_string(),
            _ => "move (dry run)".to_string(),
        });
    }
    match row.kind {
        // New wins; the legacy copy parks.
        Kind::Marker | Kind::Page if legacy_exists && new_exists => park_file(root, stamp, &legacy),
        Kind::Overwrite if legacy_exists && new_exists => {
            let legacy_newer = mtime_of(&legacy) > mtime_of(&new);
            if legacy_newer {
                if std::fs::rename(&legacy, &new).is_err() {
                    return Status::Refused("rename over new failed".to_string());
                }
                Status::Moved
            } else {
                park_file(root, stamp, &legacy)
            }
        }
        // Rename keeps open writers on the same inode (launchd fds), so an
        // append merge must copy bytes, never re-open through the old name.
        Kind::Append if legacy_exists && new_exists => match append_merge(&legacy, &new) {
            Ok(()) => match park_file(root, stamp, &legacy) {
                Status::Parked => Status::Merged,
                other => other,
            },
            Err(e) => Status::Refused(format!("merge failed: {e}")),
        },
        _ => match move_entry(&legacy, &new) {
            Ok(()) => Status::Moved,
            Err(e) => Status::Refused(format!("refused: {e}")),
        },
    }
}

/// A legacy lock leaves only after its data file moved (so no waiter still
/// needs it) and a non-blocking flock succeeds (so no holder still owns it).
/// A socket never flocks; it parks once its store moved.
fn migrate_lock(root: &Path, row: &Row, apply: bool, stamp: &str) -> Status {
    let legacy = root.join(&row.legacy);
    if !legacy.exists() {
        return Status::Absent;
    }
    let base = row.legacy.strip_suffix(".lock").unwrap_or(&row.legacy);
    let data_moved = match find(base) {
        Some(base_row) => root.join(&base_row.new).exists(),
        None => false,
    };
    if !data_moved {
        return Status::Pending(format!("waiting for {base} to move"));
    }
    if !apply {
        return Status::Pending("lock removal (dry run)".to_string());
    }
    let is_socket = std::fs::symlink_metadata(&legacy)
        .map(|m| m.file_type().is_socket())
        .unwrap_or(false);
    if is_socket {
        return park_file(root, stamp, &legacy);
    }
    match std::fs::File::open(&legacy) {
        Ok(f) => {
            let rc = unsafe {
                libc::flock(
                    std::os::unix::io::AsRawFd::as_raw_fd(&f),
                    libc::LOCK_EX | libc::LOCK_NB,
                )
            };
            if rc != 0 {
                return Status::Pending("lock held".to_string());
            }
            match std::fs::remove_file(&legacy) {
                Ok(()) => Status::Deleted,
                Err(e) => Status::Refused(format!("refused: {e}")),
            }
        }
        Err(e) => Status::Refused(format!("refused: {e}")),
    }
}

fn move_entry(legacy: &Path, new: &Path) -> std::io::Result<()> {
    if let Some(parent) = new.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(legacy, new)
}

/// Legacy bytes then new bytes, through a tmp file in the new folder, fsync,
/// rename over the new file. The new file's mode carries over (same dir).
fn append_merge(legacy: &Path, new: &Path) -> std::io::Result<()> {
    let perms = std::fs::metadata(new)?.permissions();
    let tmp = new.with_extension(format!("migrating.{}", std::process::id()));
    let mut dst = std::fs::File::create(&tmp)?;
    // Streamed: the logs this merges are documented unbounded, and a full
    // read would spike the daemon's memory by both files' size.
    std::io::copy(&mut std::fs::File::open(legacy)?, &mut dst)?;
    std::io::copy(&mut std::fs::File::open(new)?, &mut dst)?;
    dst.sync_all()?;
    std::fs::set_permissions(&tmp, perms)?;
    std::fs::rename(&tmp, new)?;
    Ok(())
}

fn mtime_of(p: &Path) -> std::time::SystemTime {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .unwrap_or(std::time::UNIX_EPOCH)
}

/// The run's backup folder: `backups/state-root-migration/<utc stamp>/`.
fn backup_dir(root: &Path, stamp: &str) -> PathBuf {
    root.join("backups")
        .join("state-root-migration")
        .join(stamp)
}

fn park_entry(root: &Path, stamp: &str, name: &str) -> Status {
    let p = root.join(name);
    if !p.exists() {
        return Status::Absent;
    }
    park_file(root, stamp, &p)
}

/// Rename one legacy path into the run's backup folder. Never deletes: a
/// name collision in the same stamp gains `.1`, `.2`, ...
fn park_file(root: &Path, stamp: &str, legacy: &Path) -> Status {
    let dir = backup_dir(root, stamp);
    let Some(name) = legacy.file_name() else {
        return Status::Refused("refused: no file name".to_string());
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Status::Refused(format!("refused: {e}"));
    }
    let mut target = dir.join(name);
    let mut n = 0;
    while target.exists() {
        n += 1;
        let mut os = name.to_os_string();
        os.push(format!(".{n}"));
        target = dir.join(os);
    }
    match std::fs::rename(legacy, &target) {
        Ok(()) => Status::Parked,
        Err(e) => Status::Refused(format!("refused: {e}")),
    }
}

/// Single-`*` glob match: prefix and suffix around the star. Enough for the
/// table's residue rows; no dependency.
fn glob_matches(root: &Path, pattern: &str) -> Vec<String> {
    let Some((prefix, suffix)) = pattern.split_once('*') else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let Ok(read) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in read.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.len() >= prefix.len() + suffix.len()
            && name.starts_with(prefix)
            && name.ends_with(suffix)
        {
            out.push(name.to_string());
        }
    }
    out.sort();
    out
}

fn utc_stamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Civil-from-days (Howard Hinnant's algorithm); no chrono dependency.
    let days = (now / 86_400) as i64;
    let secs = (now % 86_400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        y,
        m,
        d,
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// The daemon bin calls this once at startup, BEFORE `daemon::run` opens a
/// store: one apply pass over the state root. Best effort - a refusal only
/// prints, and the daemon's reclaim lane retries it (law: the mover runs
/// from the daemon, never an operator verb).
pub fn run_at_daemon_start(home: &crate::paths::AgentsHome) {
    let root = crate::reclaim::reclaim_state_root(home);
    let receipt = migrate(&root, true);
    if receipt.refused_count() > 0 {
        eprintln!(
            "fno-agents-daemon: state-root migration: {}",
            receipt.summary()
        );
    }
}

/// `fno-agents state migrate [--apply] [--json|-J]`: dry run by default.
/// Resolves the state root the way every other state-root reader does.
pub fn run_migrate_cli(args: &[String]) -> i32 {
    let mut apply = false;
    let json = crate::json_output::requested(args);
    for arg in args {
        match arg.as_str() {
            "--apply" => apply = true,
            other if crate::json_output::is_flag(other) => {}
            other => {
                eprintln!("fno-agents state migrate: unknown arg: {other}");
                return 2;
            }
        }
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let root = crate::agents_config::state_dir(&cwd).unwrap_or_else(|| {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        home.join(".fno")
    });
    let receipt = migrate(&root, apply);
    if json {
        let entries: Vec<String> = receipt
            .entries
            .iter()
            .map(|e| {
                let detail = match e.status.detail() {
                    Some(d) => format!(",\"detail\":\"{}\"", json_escape(d)),
                    None => String::new(),
                };
                format!(
                    "{{\"legacy\":\"{}\",\"status\":\"{}\"{}}}",
                    json_escape(&e.legacy),
                    e.status.word(),
                    detail
                )
            })
            .collect();
        println!(
            "{{\"root\":\"{}\",\"pending\":{},\"entries\":[{}]}}",
            json_escape(&root.display().to_string()),
            receipt.pending_count(),
            entries.join(",")
        );
    } else {
        for e in &receipt.entries {
            match e.status.detail() {
                Some(d) => println!("  {}: {} ({})", e.status.word(), e.legacy, d),
                None if matches!(e.status, Status::Pending(_)) => {
                    println!("  pending: {}", e.legacy)
                }
                None => println!("  {}: {}", e.status.word(), e.legacy),
            }
        }
        println!(
            "state migrate ({}): {}",
            if apply { "applied" } else { "dry run" },
            receipt.summary()
        );
    }
    if receipt.refused_count() > 0 {
        1
    } else {
        0
    }
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn tmp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fno-state-layout-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn clean(root: &Path) {
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn shipped_table_parses() {
        let rows = parse_table(LAYOUT_TSV).expect("shipped table must parse");
        assert!(
            rows.len() > 40,
            "expected the full table, got {}",
            rows.len()
        );
        for row in rows {
            if row.kind == Kind::Park {
                assert_eq!(row.new, "-");
            } else {
                assert!(
                    row.new.contains('/'),
                    "row {} must land in a subfolder",
                    row.legacy
                );
            }
        }
    }

    #[test]
    fn parse_rejects_wrong_column_count_naming_the_line() {
        let tsv = "# comment\n\ngood\tnew/good\tmarker\tdaemon\nbad\tnew/bad\tmarker\n";
        let err = parse_table(tsv).unwrap_err();
        assert!(err.contains("line 4"), "error must name the line: {err}");
    }

    #[test]
    fn parse_rejects_unknown_kind_naming_the_line() {
        let tsv = "a\tx/a\tmarker\tdaemon\nb\tx/b\tvault\tdaemon\n";
        let err = parse_table(tsv).unwrap_err();
        assert!(err.contains("line 2"), "error must name the line: {err}");
    }

    #[test]
    fn place_ac2() {
        let root = tmp_root("place");
        assert_eq!(
            place(&root, "graph.json"),
            root.join("db").join("graph.json")
        );
        std::fs::write(root.join("graph.db"), b"x").unwrap();
        assert_eq!(place(&root, "graph.json"), root.join("graph.json"));
        std::fs::create_dir_all(root.join("db")).unwrap();
        std::fs::write(root.join("db").join("graph.db"), b"x").unwrap();
        assert_eq!(
            place(&root, "graph.json"),
            root.join("db").join("graph.json")
        );
        // A table miss resolves through the root unchanged.
        assert_eq!(place(&root, "other.json"), root.join("other.json"));
        clean(&root);
    }

    #[test]
    fn place_falls_back_for_plain_kinds() {
        let root = tmp_root("plain");
        std::fs::write(root.join("installed-rev"), b"rev").unwrap();
        assert_eq!(place(&root, "installed-rev"), root.join("installed-rev"));
        std::fs::create_dir_all(root.join("install")).unwrap();
        std::fs::write(root.join("install").join("installed-rev"), b"rev").unwrap();
        assert_eq!(
            place(&root, "installed-rev"),
            root.join("install").join("installed-rev")
        );
        clean(&root);
    }

    #[test]
    fn append_merge_ac3_hp() {
        let root = tmp_root("merge");
        std::fs::write(root.join("pr-watcher.out.log"), b"legacy-1\nlegacy-2\n").unwrap();
        std::fs::create_dir_all(root.join("logs")).unwrap();
        std::fs::write(root.join("logs").join("pr-watcher.out.log"), b"new-1\n").unwrap();
        let receipt = migrate(&root, true);
        let text = std::string::String::from_utf8(
            std::fs::read(root.join("logs").join("pr-watcher.out.log")).unwrap(),
        )
        .unwrap();
        assert_eq!(text, "legacy-1\nlegacy-2\nnew-1\n");
        assert!(!root.join("pr-watcher.out.log").exists(), "legacy parked");
        let parked = std::fs::read_dir(root.join("backups").join("state-root-migration"))
            .unwrap()
            .next()
            .is_some();
        assert!(
            parked,
            "a stamp folder exists under backups/state-root-migration"
        );
        let found = receipt
            .entries
            .iter()
            .find(|e| e.legacy == "pr-watcher.out.log")
            .unwrap();
        assert!(matches!(found.status, Status::Merged));
        clean(&root);
    }

    #[test]
    fn marker_move_parks_residue_rows() {
        let root = tmp_root("marker");
        std::fs::write(root.join("installed-rev"), b"abc\n").unwrap();
        std::fs::write(root.join("MOVED-TO"), b"somewhere\n").unwrap();
        let receipt = migrate(&root, true);
        assert!(root.join("install").join("installed-rev").exists());
        assert!(!root.join("installed-rev").exists());
        let found = receipt
            .entries
            .iter()
            .find(|e| e.legacy == "installed-rev")
            .unwrap();
        assert!(matches!(found.status, Status::Moved));
        let parked = receipt
            .entries
            .iter()
            .find(|e| e.legacy == "MOVED-TO")
            .unwrap();
        assert!(matches!(parked.status, Status::Parked));
        clean(&root);
    }

    #[test]
    fn overwrite_newer_mtime_wins() {
        // New is newer: legacy parks, new bytes stay.
        let root = tmp_root("ow-new");
        std::fs::create_dir_all(root.join("state")).unwrap();
        std::fs::write(root.join("notify-signals.json"), b"legacy").unwrap();
        let new_path = root.join("state").join("notify-signals.json");
        std::fs::write(&new_path, b"new").unwrap();
        let newer = std::time::SystemTime::now() + std::time::Duration::from_secs(90);
        set_mtime(&new_path, newer);
        let receipt = migrate(&root, true);
        assert_eq!(std::fs::read(&new_path).unwrap(), b"new");
        assert!(!root.join("notify-signals.json").exists());
        let found = receipt
            .entries
            .iter()
            .find(|e| e.legacy == "notify-signals.json")
            .unwrap();
        assert!(matches!(found.status, Status::Parked));
        clean(&root);
        // Legacy is newer: its bytes land on the new path.
        let root = tmp_root("ow-legacy");
        std::fs::create_dir_all(root.join("state")).unwrap();
        std::fs::write(root.join("notify-signals.json"), b"legacy").unwrap();
        let new_path = root.join("state").join("notify-signals.json");
        std::fs::write(&new_path, b"new").unwrap();
        let newer = std::time::SystemTime::now() + std::time::Duration::from_secs(90);
        set_mtime(root.join("notify-signals.json").as_path(), newer);
        let _ = migrate(&root, true);
        assert_eq!(std::fs::read(&new_path).unwrap(), b"legacy");
        clean(&root);
        // The sqlite kind rides the same move rules through the backup-API
        // protocol (state_layout_sqlite). Branch: committed rows still in
        // the -wal land in the published copy and the trio parks.
        let root = tmp_root("ow-sq-wal");
        let legacy = root.join("graph.db");
        let conn = rusqlite::Connection::open(&legacy).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE layout_nodes (id TEXT PRIMARY KEY, title TEXT);
             INSERT INTO layout_nodes VALUES ('x-1','one');",
        )
        .unwrap();
        let row = parse_table("graph.db\tdb/graph.db\tsqlite\tdaemon\n")
            .unwrap()
            .remove(0);
        let status = crate::state_layout_sqlite::migrate_sqlite_row(&root, &row, true, "ow-wal");
        assert!(matches!(status, Status::Moved), "{status:?}");
        let new_db = root.join("db").join("graph.db");
        let c = rusqlite::Connection::open(&new_db).unwrap();
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM layout_nodes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "the committed row moved");
        assert!(!legacy.exists(), "the legacy file parked");
        assert!(root
            .join("backups")
            .join("state-root-migration")
            .join("ow-wal")
            .join("graph.db")
            .exists());
        drop(c);
        clean(&root);
        // Branch: a writer holding BEGIN IMMEDIATE past the busy timeout
        // parks the row, leaves the trio untouched, and clears the fence.
        let root = tmp_root("ow-sq-busy");
        let legacy = root.join("graph.db");
        let conn = rusqlite::Connection::open(&legacy).unwrap();
        conn.execute_batch("CREATE TABLE t (a); INSERT INTO t VALUES (1);")
            .unwrap();
        conn.execute_batch("BEGIN IMMEDIATE;").unwrap();
        let status = crate::state_layout_sqlite::migrate_sqlite_row(&root, &row, true, "ow-busy");
        assert!(
            matches!(status, Status::Pending(_)),
            "expected pending, got {status:?}"
        );
        assert!(legacy.exists(), "the legacy trio is untouched");
        assert!(!crate::state_layout_sqlite::fence_path(&root).exists());
        conn.execute_batch("ROLLBACK;").unwrap();
        drop(conn);
        clean(&root);
        // Branch: rows on both sides refuse and keep both copies.
        let root = tmp_root("ow-sq-both");
        let legacy = root.join("graph.db");
        let conn = rusqlite::Connection::open(&legacy).unwrap();
        conn.execute_batch("CREATE TABLE t (a); INSERT INTO t VALUES (1);")
            .unwrap();
        drop(conn);
        std::fs::create_dir_all(root.join("db")).unwrap();
        let new_db = root.join("db").join("graph.db");
        let conn = rusqlite::Connection::open(&new_db).unwrap();
        conn.execute_batch("CREATE TABLE t (a); INSERT INTO t VALUES (2);")
            .unwrap();
        drop(conn);
        let status = crate::state_layout_sqlite::migrate_sqlite_row(&root, &row, true, "ow-both");
        assert!(matches!(status, Status::Refused(_)), "{status:?}");
        assert!(legacy.exists() && new_db.exists(), "both copies stay");
        clean(&root);
        // Branch: an empty new copy is a fresh-create race; it parks and the
        // legacy rows win.
        let root = tmp_root("ow-sq-empty");
        let legacy = root.join("graph.db");
        let conn = rusqlite::Connection::open(&legacy).unwrap();
        conn.execute_batch("CREATE TABLE t (a); INSERT INTO t VALUES (1);")
            .unwrap();
        drop(conn);
        std::fs::create_dir_all(root.join("db")).unwrap();
        let new_db = root.join("db").join("graph.db");
        rusqlite::Connection::open(&new_db)
            .unwrap()
            .execute_batch("CREATE TABLE z (a);")
            .unwrap();
        let status = crate::state_layout_sqlite::migrate_sqlite_row(&root, &row, true, "ow-empty");
        assert!(matches!(status, Status::Moved), "{status:?}");
        let c = rusqlite::Connection::open(&new_db).unwrap();
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "the legacy rows won");
        drop(c);
        // Branch: the deferred verify compares a parked copy against its
        // park-time counts once the record ages past the delay. Same root:
        // the ow-empty stamp's record is still in place.
        let record = root
            .join("backups")
            .join("state-root-migration")
            .join("ow-empty")
            .join("verify.tsv");
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&record)
            .unwrap();
        f.set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(60)),
        )
        .unwrap();
        drop(f);
        let swept = crate::state_layout_sqlite::verify_sweep(&root, "ow-later");
        assert_eq!(swept.len(), 1, "{swept:?}");
        assert!(matches!(swept[0].1, Status::Moved), "{swept:?}");
        clean(&root);
    }

    fn set_mtime(p: &Path, t: std::time::SystemTime) {
        let f = std::fs::File::options().write(true).open(p).unwrap();
        f.set_modified(t).unwrap();
    }

    #[test]
    fn park_glob_rows_match_and_park() {
        let root = tmp_root("glob");
        std::fs::write(root.join("groom-2026-09-26.md"), b"r1").unwrap();
        std::fs::write(root.join("groom-2020-01-01.md"), b"r2").unwrap();
        std::fs::write(root.join("unrelated.md"), b"keep").unwrap();
        let receipt = migrate(&root, true);
        assert!(!root.join("groom-2026-09-26.md").exists());
        assert!(!root.join("groom-2020-01-01.md").exists());
        assert!(
            root.join("unrelated.md").exists(),
            "non-matching names stay"
        );
        let groom = receipt
            .entries
            .iter()
            .filter(|e| e.legacy.starts_with("groom-"))
            .count();
        assert_eq!(groom, 2, "both groom reports parked");
        clean(&root);
    }

    #[test]
    fn mux_and_sqlite_rows_report_pending() {
        let root = tmp_root("pending");
        std::fs::write(root.join("mux-view.json"), b"view").unwrap();
        std::fs::write(root.join("graph.db"), b"SQLite format 3").unwrap();
        std::fs::write(root.join("graph.json.lock"), b"").unwrap();
        std::fs::write(root.join("squads.json.tmp.77"), b"t").unwrap();
        let receipt = migrate(&root, false);
        assert_eq!(receipt.pending_count(), 4);
        let tmp = receipt
            .entries
            .iter()
            .find(|e| e.legacy == "squads.json.tmp.*")
            .unwrap();
        assert!(
            matches!(tmp.status, Status::Pending(ref d) if d.contains("mux")),
            "the mux owner parks its own tmp residue: {:?}",
            tmp.status
        );
        for name in ["events.db", "decisions.db", "questions.db", "approvals.db"] {
            let row = find(name).unwrap();
            assert_eq!(row.new, name);
            for suffix in ["", "-wal", "-shm"] {
                let path = root.join(format!("{name}{suffix}"));
                std::fs::write(&path, b"retained store bytes").unwrap();
                if let Some(sidecar) = find(&format!("{name}{suffix}")) {
                    for apply in [false, true] {
                        assert!(matches!(
                            crate::state_layout_sqlite::migrate_sqlite_row(
                                &root, sidecar, apply, "identity"
                            ),
                            Status::Moved
                        ));
                        assert_eq!(std::fs::read(&path).unwrap(), b"retained store bytes");
                    }
                }
            }
        }
        clean(&root);
    }

    #[test]
    fn lock_waits_for_its_data_file() {
        let root = tmp_root("lock");
        std::fs::write(root.join("graph.json.lock"), b"").unwrap();
        let receipt = migrate(&root, true);
        assert!(root.join("graph.json.lock").exists(), "lock untouched");
        let found = receipt
            .entries
            .iter()
            .find(|e| e.legacy == "graph.json.lock")
            .unwrap();
        assert!(matches!(found.status, Status::Pending(ref d) if d.contains("graph.json")));
        clean(&root);
    }

    #[test]
    fn second_apply_is_idempotent() {
        let root = tmp_root("idem");
        std::fs::write(root.join("installed-rev"), b"abc\n").unwrap();
        let first = migrate(&root, true);
        let first_parks = first
            .entries
            .iter()
            .filter(|e| matches!(e.status, Status::Parked))
            .count();
        let second = migrate(&root, 2 == 2);
        let second_parks = second
            .entries
            .iter()
            .filter(|e| matches!(e.status, Status::Parked))
            .count();
        assert_eq!(first_parks, 0);
        assert_eq!(second_parks, 0);
        let moved = second
            .entries
            .iter()
            .find(|e| e.legacy == "installed-rev")
            .unwrap();
        assert!(
            matches!(moved.status, Status::Moved),
            "already at the new path reads moved"
        );
        clean(&root);
    }

    #[test]
    fn refused_when_rename_fails() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let root = tmp_root("refuse");
        std::fs::write(root.join("installed-rev"), b"abc\n").unwrap();
        let restore = std::fs::metadata(&root).unwrap().permissions();
        let mut locked = restore.clone();
        use std::os::unix::fs::PermissionsExt;
        locked.set_mode(0o500);
        std::fs::set_permissions(&root, locked).unwrap();
        let receipt = migrate(&root, true);
        std::fs::set_permissions(&root, restore).unwrap();
        let found = receipt
            .entries
            .iter()
            .find(|e| e.legacy == "installed-rev")
            .unwrap();
        assert!(
            matches!(found.status, Status::Refused(ref d) if d.contains("refused:")),
            "{:?}",
            found.status
        );
        assert!(root.join("installed-rev").exists(), "legacy untouched");
        clean(&root);
    }
    #[test]
    fn vendored_table_matches_the_repo_copy() {
        let repo = include_str!("../../../docs/state-root-layout.tsv");
        assert_eq!(
            LAYOUT_TSV, repo,
            "the vendored layout table drifted from docs/state-root-layout.tsv;              edit the repo copy and copy it into both crates"
        );
    }
}
