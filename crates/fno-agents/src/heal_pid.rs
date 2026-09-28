//! The pr-heal drive loop's pid file: where it lives, who is in flight, and
//! who cleans up. Extracted from heal.rs so the in-flight question has one
//! small owner; heal.rs keeps thin adapters over these.

use std::path::{Path, PathBuf};

/// The pid file for THE drive loop, in its own subfolder: nothing writes at
/// the top level of the state root. One process heals every root, so one
/// pid file: the in-flight guard sees the loop whatever root asked.
pub fn pid_file(events_dir: &Path) -> PathBuf {
    events_dir.join("heal").join("pr-heal.pid")
}

/// Test-only reader since the guard switched to live pids.
#[cfg_attr(not(test), allow(dead_code))]
pub fn read_pid_file(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// True when the pid names a healer process. A stale pid file outlives its
/// loop, and pid numbers are recycled: without an identity check a recycled
/// pid holds the tick (every root) for as long as the unrelated owner lives.
/// `ps -o command=` answers on macOS and Linux alike; an unreadable answer
/// counts as NOT the healer (fail open to a spawn, never stuck in_flight).
/// The current process is exempt: production never writes its own pid to a
/// file, and the test harness does exactly that.
fn pid_names_a_healer(pid: u32) -> bool {
    if pid == std::process::id() {
        return true;
    }
    let Ok((true, out, _)) = crate::pr_push::run_labeled(
        "pr-heal",
        "ps",
        &["-p", &pid.to_string(), "-o", "command="],
        &std::env::temp_dir(),
        crate::pr_push::READ_TIMEOUT,
    ) else {
        return false;
    };
    out.contains("pr-heal") || out.contains("fno-agents")
}

/// Live pids across every `pr-heal.*.pid` file in `events_dir` and its
/// `heal/` subfolder. The pid lives in the file CONTENT, read as an integer;
/// EPERM counts alive. The glob also sweeps the old per-root
/// `pr-heal.<tag>.pid` files; a file whose pid is dead is the family's own
/// litter and is deleted here (the reader is the deleter: nothing else owns
/// these files), and a recycled pid that belongs to an unrelated process
/// skips without deleting.
pub fn live_pids(events_dir: &Path) -> Vec<u32> {
    let mut out = Vec::new();
    for scan_dir in [events_dir.join("heal"), events_dir.to_path_buf()] {
        let Ok(entries) = std::fs::read_dir(&scan_dir) else {
            continue;
        };
        for e in entries.flatten() {
            let fname = e.file_name();
            let Some(name) = fname.to_str() else {
                continue;
            };
            if !name.starts_with("pr-heal.") || !name.ends_with(".pid") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(e.path()) else {
                continue;
            };
            let Ok(pid) = text.trim().parse::<u32>() else {
                continue;
            };
            if crate::evals_arm::pid_alive(pid) && pid_names_a_healer(pid) {
                out.push(pid);
            } else {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    out.sort_unstable();
    out
}
