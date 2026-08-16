//! The per-agent flock at `<registry-dir>/locks/<name>.lock`.
//!
//! ONE implementation, deliberately. This lived as three byte-identical
//! copies in `claude_ask`, `codex_ask` and `gemini_ask`, and the holder stamp
//! landed on one of them first. A stamp on one of N reachable implementations
//! is decorative: the other two leave a previous holder's JSON in the file, so
//! a waiter reports a dead pid as the live owner. Any harness that takes this
//! lock must take it through here.
//!
//! Not every harness does. `agy_ask` and `opencode_ask` acquire no per-agent
//! lock at all, so concurrent same-agent asks on those two do NOT serialize
//! (pre-existing; unchanged by the consolidation above). Wiring them up is a
//! behavior change for those lanes, not a refactor, so it is not done here.
//!
//! Byte-compatible with the Python side (`fno.agents.lock`), which takes the
//! same flock on the same path and parses the stamp written below.

use std::time::{Duration, Instant};

use crate::paths::AgentsHome;

/// RAII per-agent flock. The lock releases when this drops, and also when the
/// process exits: a POSIX flock belongs to the open file description, never to
/// the file's contents. A stale mtime on this path is not a held lock.
pub(crate) struct AgentLock {
    _file: std::fs::File,
}

fn lock_path(home: &AgentsHome, name: &str) -> std::path::PathBuf {
    home.root().join("locks").join(format!("{}.lock", name))
}

/// `" (held by pid N since T)"`, or `""` when the lock carries no readable
/// stamp. Mirror of Python's `AgentLockTimeout.holder_note`.
///
/// Stamping on every acquire and reading on no timeout path would leave the
/// stamp decorative for every harness that takes the lock through Rust: the
/// waiter that needs the holder's name is exactly the one that never gets it.
pub(crate) fn holder_note(home: &AgentsHome, name: &str) -> String {
    let raw = match std::fs::read_to_string(lock_path(home, name)) {
        Ok(raw) => raw,
        Err(_) => return String::new(),
    };
    let parsed: serde_json::Value = match serde_json::from_str(raw.lines().next().unwrap_or("")) {
        Ok(value) => value,
        Err(_) => return String::new(),
    };
    match (
        parsed.get("pid").and_then(serde_json::Value::as_u64),
        parsed
            .get("acquired_at")
            .and_then(serde_json::Value::as_str),
    ) {
        (Some(pid), Some(at)) if !at.is_empty() && pid_is_alive(pid) => {
            format!(" (held by pid {pid} since {at})")
        }
        _ => String::new(),
    }
}

/// True when `pid` names a live process. `EPERM` means it exists under another
/// uid, so that counts as alive.
///
/// `stamp_holder` cannot clear a stamp when its own `set_len` fails, so a
/// previous holder's line can outlive it. Naming a dead pid is the
/// 31-hour-corpse misreading this stamp exists to end, told with more
/// authority than the bare mtime ever had, so the reader drops it.
fn pid_is_alive(pid: u64) -> bool {
    if pid == 0 || pid > i32::MAX as u64 {
        return false;
    }
    // SAFETY: signal 0 performs the permission and existence check only; it
    // delivers nothing and cannot affect the target.
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Write this process as the lock's holder, in the shape the Python reader
/// (`fno.agents.lock._read_holder`) parses.
///
/// Truncating is the load-bearing half, and it is the half that can fail on
/// its own: a `set_len` error returns here with an earlier holder's line still
/// in the file. `holder_note` covers that residue by dropping a stamp whose
/// pid is dead. Best-effort throughout: the flock is held either way and the
/// stamp is diagnostic only.
fn stamp_holder(file: &std::fs::File, name: &str) {
    use std::io::{Seek, SeekFrom, Write};

    let mut handle = file;
    if handle.set_len(0).is_err() {
        return;
    }
    let _ = handle.seek(SeekFrom::Start(0));
    let line = serde_json::json!({
        "pid": std::process::id(),
        "name": name,
        "acquired_at": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    });
    let _ = writeln!(handle, "{}", line);
    let _ = handle.flush();
}

impl AgentLock {
    pub(crate) fn acquire(home: &AgentsHome, name: &str, timeout: Duration) -> Result<Self, ()> {
        let _ = std::fs::create_dir_all(home.root().join("locks"));
        let path = lock_path(home, name);
        // truncate(false) so opening never clears a stamp the current holder
        // wrote; the stamp is replaced only after this process wins the lock.
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|_| ())?;
        let deadline = Instant::now() + timeout;
        loop {
            match file.try_lock() {
                Ok(()) => {
                    stamp_holder(&file, name);
                    return Ok(Self { _file: file });
                }
                Err(_) => {
                    if Instant::now() >= deadline {
                        return Err(());
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
        }
    }
}

impl Drop for AgentLock {
    fn drop(&mut self) {
        // std's inherent File::unlock (stable since Rust 1.89; the crate pins
        // rust-version = 1.89). Mirrors acquire()'s std locking.
        let _ = self._file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A Python waiter that times out reads this file to name the holder. When
    /// acquire leaves an earlier holder's JSON in place, it reports a dead pid
    /// as the live owner, which is the lie the stamp exists to remove.
    #[test]
    fn acquire_replaces_any_earlier_holder_stamp() {
        let dir = std::env::temp_dir().join(format!("fno-lock-stamp-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("locks")).unwrap();
        let lock_path = dir.join("locks").join("red.lock");
        fs::write(
            &lock_path,
            "{\"pid\": 999999, \"name\": \"red\", \"acquired_at\": \"1999-01-01T00:00:00Z\"}\n",
        )
        .unwrap();

        let home = AgentsHome::at(&dir);
        {
            let _lock = AgentLock::acquire(&home, "red", Duration::from_secs(2)).unwrap();
            let raw = fs::read_to_string(&lock_path).unwrap();
            assert_eq!(raw.lines().count(), 1, "stamp must stay one line: {raw}");
            let parsed: serde_json::Value = serde_json::from_str(raw.trim()).unwrap();
            assert_eq!(
                parsed["pid"].as_u64().unwrap(),
                u64::from(std::process::id())
            );
            assert_eq!(parsed["name"].as_str().unwrap(), "red");
            assert_ne!(
                parsed["acquired_at"].as_str().unwrap(),
                "1999-01-01T00:00:00Z"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// The stamp outlives its writer only when `stamp_holder`'s truncate failed
    /// or the holder died mid-write. Reporting that pid as the live owner is
    /// the corpse-reading the stamp replaced, so the reader must refuse it.
    #[test]
    fn holder_note_drops_a_dead_pid_and_keeps_a_live_one() {
        let dir = std::env::temp_dir().join(format!("fno-lock-note-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("locks")).unwrap();
        let home = AgentsHome::at(&dir);

        // pid 0 is never a real holder; the u64 guard rejects it outright.
        fs::write(
            dir.join("locks").join("ghost.lock"),
            "{\"pid\": 0, \"name\": \"ghost\", \"acquired_at\": \"1999-01-01T00:00:00Z\"}\n",
        )
        .unwrap();
        assert_eq!(holder_note(&home, "ghost"), "");

        fs::write(
            dir.join("locks").join("live.lock"),
            format!(
                "{{\"pid\": {}, \"name\": \"live\", \"acquired_at\": \"1999-01-01T00:00:00Z\"}}\n",
                std::process::id()
            ),
        )
        .unwrap();
        let note = holder_note(&home, "live");
        assert!(
            note.contains(&format!("held by pid {}", std::process::id())),
            "a live holder must still be named: {note}"
        );

        // No stamp at all degrades to the bare message rather than panicking.
        assert_eq!(holder_note(&home, "absent"), "");
        let _ = fs::remove_dir_all(&dir);
    }
}
