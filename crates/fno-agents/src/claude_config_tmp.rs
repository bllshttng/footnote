//! Orphaned Claude Code atomic-write temp files: `.claude.json.tmp.<pid>.<hex>`
//! dropped at the top level of each Claude config dir. Claude Code writes its
//! config through a temp file it renames into place, so a claude process
//! killed mid-write leaves the temp behind (46 files, 2.2 MB, measured
//! 2026-09-27). The reclaim lane removes one only when BOTH guards hold: the
//! file is older than [`TMP_MIN_AGE`] and the pid in its name is dead. A live
//! pid may be mid-rename; a fresh file may still be writing; a name without a
//! pid is never ours to judge.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// How old a temp file must be before a dead pid reads as abandoned and not
/// as a write still settling.
pub const TMP_MIN_AGE: Duration = Duration::from_secs(3600);

fn file_name_prefix() -> &'static str {
    ".claude.json.tmp."
}

/// The pid encoded in `.claude.json.tmp.<pid>.<hex>`; `None` for any other
/// name shape, which the sweep then never touches.
pub fn tmp_pid(name: &str) -> Option<u32> {
    let rest = name.strip_prefix(file_name_prefix())?;
    rest.split('.').next()?.parse::<u32>().ok()
}

/// A pid the kernel has never heard of is dead; EPERM means it exists and is
/// someone else's, which is as alive as it gets for this purpose.
pub fn pid_alive(pid: u32) -> bool {
    // SAFETY: signal 0 is the existence probe; it delivers nothing.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// The Claude config dirs this machine knows: the ambient root
/// (`CLAUDE_CONFIG_DIR`, else `~/.claude`) plus every isolated account root
/// the roster mirrors from the accounts config, deduplicated.
pub fn claude_config_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(cfg) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()) {
        dirs.push(PathBuf::from(cfg));
    } else if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".claude"));
    }
    dirs.extend(
        crate::claude_roster::isolated_account_dirs()
            .into_iter()
            .map(|(_, dir)| dir),
    );
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| seen.insert(std::fs::canonicalize(d).unwrap_or_else(|_| d.clone())));
    dirs
}

#[derive(Debug)]
pub struct TmpRow {
    pub path: PathBuf,
    pub pid: Option<u32>,
    pub bytes: u64,
    pub reaped: bool,
    pub reason: String,
}

#[derive(Debug, Default)]
pub struct TmpReport {
    pub rows: Vec<TmpRow>,
    pub reaped: Vec<PathBuf>,
    pub bytes_reaped: u64,
    pub kept: usize,
}

impl TmpReport {
    pub fn note(&self, apply: bool) -> String {
        let verb = if apply { "removed" } else { "would remove" };
        format!(
            "{verb} {} orphaned .claude.json.tmp file(s), kept {} (live pid, fresh, or unparsable)",
            self.reaped.len(),
            self.kept
        )
    }
}

/// One pass over every config dir. Dry run by default: `apply=false` reports
/// what `apply=true` would remove and removes nothing.
pub fn sweep(apply: bool, now: SystemTime) -> TmpReport {
    sweep_with_min_age(apply, now, TMP_MIN_AGE)
}

/// The sweep with the age floor injected, so tests can exercise both sides of
/// the guard without backdating mtimes.
pub fn sweep_with_min_age(apply: bool, now: SystemTime, min_age: Duration) -> TmpReport {
    let mut rep = TmpReport::default();
    for dir in claude_config_dirs() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with(file_name_prefix()) {
                continue;
            }
            let path = dir.join(&*name);
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(m) if m.is_file() => m,
                _ => continue,
            };
            let pid = tmp_pid(&name);
            let age = now
                .duration_since(meta.modified().unwrap_or(now))
                .unwrap_or(Duration::ZERO);
            let mut removed = false;
            let reason = if pid.is_none() {
                "kept: no pid in name".to_string()
            } else if pid_alive(pid.unwrap()) {
                "kept: pid alive".to_string()
            } else if age < min_age {
                format!(
                    "kept: age {}s is under the {}s floor",
                    age.as_secs(),
                    min_age.as_secs()
                )
            } else if !apply {
                // Eligible but reported only: the dry run names the file in
                // `reaped` so the lane prints what --apply would remove.
                "dry-run: pass --apply to remove".to_string()
            } else {
                match std::fs::remove_file(&path) {
                    Ok(()) => {
                        removed = true;
                        "removed".to_string()
                    }
                    Err(e) => format!("kept: remove failed: {e}"),
                }
            };
            let bytes = meta.len();
            if removed || reason.starts_with("dry-run") {
                rep.reaped.push(path.clone());
                rep.bytes_reaped += bytes;
            } else {
                rep.kept += 1;
            }
            rep.rows.push(TmpRow {
                path,
                pid,
                bytes,
                reaped: removed,
                reason,
            });
        }
    }
    rep
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("fno-claude-tmp-{}-{tag}", std::process::id()))
    }

    /// A sandbox config dir with the env pinned to it, so the sweep never
    /// touches the real `~/.claude` however the ambient env reads.
    fn pinned_env(tag: &str) -> (PathBuf, PinnedConfigDir) {
        let dir = temp_dir(tag);
        std::fs::create_dir_all(&dir).unwrap();
        let guard = PinnedConfigDir::at(&dir);
        (dir, guard)
    }

    fn write_tmp(dir: &std::path::Path, pid: u32, hex: &str) -> PathBuf {
        let path = dir.join(format!(".claude.json.tmp.{pid}.{hex}"));
        std::fs::write(&path, b"x").unwrap();
        path
    }

    #[test]
    fn parses_the_pid_and_refuses_other_shapes() {
        assert_eq!(tmp_pid(".claude.json.tmp.11416.45458342395e"), Some(11416));
        assert_eq!(tmp_pid(".claude.json.tmp.1802.558fe1702408"), Some(1802));
        assert_eq!(tmp_pid(".claude.json.tmp.junk"), None);
        assert_eq!(tmp_pid(".claude.json.tmp..hex"), None);
        assert_eq!(tmp_pid("daemon.status.json"), None);
    }

    #[test]
    fn a_gone_pid_reads_dead_and_our_own_pid_reads_alive() {
        // A child we wait on is the one pid we can prove is gone.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(!pid_alive(pid));
        assert!(pid_alive(std::process::id()));
    }

    #[test]
    fn reaps_a_dead_pid_past_the_floor_and_keeps_a_live_one() {
        let (_dir, _env) = pinned_env("live-dead");
        let dir = temp_dir("live-dead");
        let dead = write_tmp(&dir, 999_999_999, "aaaaaaaaaaaa");
        let live = write_tmp(&dir, std::process::id(), "bbbbbbbbbbbb");
        let now = SystemTime::now();
        // Zero floor: age can never block, so the dead-pid guard is the only
        // one speaking - the live pid must still win.
        let rep = sweep_with_min_age(true, now, Duration::ZERO);
        assert_eq!(rep.reaped, vec![dead]);
        assert!(live.exists(), "a live pid's tmp file is never removed");
        assert_eq!(rep.kept, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_fresh_dead_pid_file_survives_the_floor() {
        let (_dir, _env) = pinned_env("fresh");
        let dir = temp_dir("fresh");
        let fresh = write_tmp(&dir, 999_999_999, "cccccccccccc");
        let rep = sweep_with_min_age(true, SystemTime::now(), Duration::from_secs(3600));
        assert!(fresh.exists());
        assert_eq!(rep.reaped.len(), 0);
        assert_eq!(rep.kept, 1);
        assert!(rep.rows[0].reason.contains("floor"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unparsable_tmp_name_is_never_touched() {
        let (_dir, _env) = pinned_env("unparsable");
        let dir = temp_dir("unparsable");
        let odd = dir.join(".claude.json.tmp.junk");
        std::fs::write(&odd, b"x").unwrap();
        let rep = sweep_with_min_age(true, SystemTime::now(), Duration::ZERO);
        assert!(odd.exists());
        assert_eq!(rep.kept, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dry_run_reports_and_removes_nothing() {
        let (_dir, _env) = pinned_env("dry");
        let dir = temp_dir("dry");
        let orphan = write_tmp(&dir, 999_999_999, "dddddddddddd");
        let rep = sweep_with_min_age(false, SystemTime::now(), Duration::ZERO);
        assert!(orphan.exists());
        // The eligible file is REPORTED (so the lane prints it) but not
        // removed: reaped carries it, the row reads reaped=false.
        assert_eq!(rep.reaped, vec![orphan.clone()]);
        assert_eq!(rep.bytes_reaped, 1);
        assert!(!rep.rows[0].reaped);
        assert!(rep.rows[0].reason.contains("dry-run"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_config_dir_names_and_dirs_are_out_of_scope() {
        let (_dir, _env) = pinned_env("scope");
        let dir = temp_dir("scope");
        let sibling = dir
            .parent()
            .unwrap()
            .join(".claude.json.tmp.999999999.eeeeeeeeeeee");
        std::fs::write(&sibling, b"x").unwrap();
        let subdir = dir.join(".claude.json.tmp.999999999.ffffffffffff");
        std::fs::create_dir_all(&subdir).unwrap();
        let rep = sweep_with_min_age(true, SystemTime::now(), Duration::ZERO);
        assert!(sibling.exists());
        assert!(subdir.exists());
        assert_eq!(rep.reaped.len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&sibling);
    }

    /// Env mutation is process-global; every pinned test holds the lock and
    /// restores the var on drop (the ENV_LOCK shape reclaim's tests use).
    struct PinnedConfigDir {
        saved: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    static ENV_LOCK: std::sync::LazyLock<&'static std::sync::Mutex<()>> =
        std::sync::LazyLock::new(crate::claims::test_env_lock);

    impl PinnedConfigDir {
        fn at(dir: &std::path::Path) -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let saved = std::env::var_os("CLAUDE_CONFIG_DIR");
            // SAFETY: every pinned test holds ENV_LOCK.
            unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", dir) };
            PinnedConfigDir { saved, _lock: lock }
        }
    }

    impl Drop for PinnedConfigDir {
        fn drop(&mut self) {
            // SAFETY: the guard holds ENV_LOCK.
            unsafe {
                match &self.saved {
                    Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
                    None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
                }
            }
        }
    }
}
