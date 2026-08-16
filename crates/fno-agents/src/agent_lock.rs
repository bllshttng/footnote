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

/// Write this process as the lock's holder, in the shape the Python reader
/// (`fno.agents.lock._read_holder`) parses.
///
/// Truncating is the load-bearing half. A holder that cannot write its own
/// identity must still not leave someone else's behind. Best-effort
/// throughout: the flock is held either way and the stamp is diagnostic only.
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
        let locks_dir = home.root().join("locks");
        let _ = std::fs::create_dir_all(&locks_dir);
        let path = locks_dir.join(format!("{}.lock", name));
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
}
