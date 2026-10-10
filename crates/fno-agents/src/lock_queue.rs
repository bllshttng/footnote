//! FIFO ordering for [`crate::graph_store::BoundedLock`] waiters.
//!
//! flock(2) wakes an arbitrary waiter, so under CPU starvation a poller can
//! lose the wakeup race repeatedly and time out while the lock is free (a
//! waiter is served, not timed out). Tickets make the order explicit: each
//! waiter writes one ticket file beside the lock and only the oldest live
//! ticket may run `try_lock`. flock stays the source of truth - a ticketless
//! writer can still take the lock out of order - so this is fairness, not
//! correctness. A waiter's ticket whose holder is gone is pruned by any scan:
//! the holder is a dead pid, a zombie (a SIGKILLed-but-unreaped holder still
//! answers kill(pid,0)), or a recycled pid (the pid + start incarnation
//! contract the lockfile stamp uses). A stalled head is bounded by its own
//! deadline, and withdrawing it serves the next.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::agent_lock::pid_is_gone;

/// Disambiguates same-nano registrations within one process; nanos already
/// separate processes (a name carries a pid).
static TICKET_SEQ: AtomicU64 = AtomicU64::new(0);

/// The ticket directory for a lock path: `<lock>.q`.
pub fn queue_dir(lock_path: &Path) -> PathBuf {
    let mut spaced = lock_path.as_os_str().to_os_string();
    spaced.push(".q");
    PathBuf::from(spaced)
}

/// Write this waiter's ticket and return its path. The zero-padded name
/// sorts chronologically, so lexicographic order is arrival order.
pub fn register(lock_path: &Path) -> std::io::Result<PathBuf> {
    let dir = queue_dir(lock_path);
    fs::create_dir_all(&dir)?;
    let nanos = chrono::Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or_default()
        .max(0) as u64;
    let name = format!(
        "{:020}-{:07}-{:04}",
        nanos,
        std::process::id(),
        TICKET_SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let path = dir.join(name);
    fs::File::create_new(&path)?;
    // The ticket carries its holder's incarnation (pid + start time, the
    // lockfile stamp's contract) so a scan can tell a zombie or a recycled
    // pid from a live waiter. Best effort: an empty ticket counts live, so a
    // scan racing this write never prunes a fresh waiter.
    let stamp = serde_json::json!({
        "pid": std::process::id(),
        "start": crate::process_probe::process_bsd(std::process::id()).map(|(start, _)| start),
    });
    let _ = fs::write(&path, format!("{stamp}"));
    Ok(path)
}

/// Remove this waiter's ticket. Best effort: a ticket left by a crashed
/// waiter is pruned by the next scan.
pub fn withdraw(ticket: Option<&Path>) {
    if let Some(t) = ticket {
        let _ = fs::remove_file(t);
    }
}

/// Whether `ticket` is the oldest live ticket in its queue. Scans prune
/// tickets whose holder is gone (dead pid, zombie, or recycled pid per the
/// stamped start time); a ticket that does not parse (a create still
/// landing) counts as live, never as absent.
pub fn am_head(ticket: &Path) -> bool {
    let Some(dir) = ticket.parent() else {
        return true;
    };
    let Some(mine) = ticket.file_name().and_then(|n| n.to_str()) else {
        return true;
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return true;
    };
    let mut head: Option<String> = None;
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name == mine {
            continue;
        }
        if let Some(pid) = ticket_pid(&name) {
            if pid_is_gone(pid, ticket_start(&entry.path())) {
                let _ = fs::remove_file(entry.path());
                continue;
            }
        }
        if head.as_deref().is_none_or(|h| name.as_str() < h) {
            head = Some(name);
        }
    }
    head.as_deref().is_none_or(|h| mine < h)
}

/// The pid on the oldest ticket in the queue for `lock_path`: the holder or
/// the waiter every later ticket queues behind. Names who blocks a timeout
/// when the lock file carries no holder stamp.
pub fn head_pid(lock_path: &Path) -> Option<u64> {
    let mut names: Vec<String> = fs::read_dir(queue_dir(lock_path))
        .ok()?
        .flatten()
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .collect();
    names.sort();
    names.first().and_then(|name| ticket_pid(name))
}

/// The pid encoded in a ticket name's middle field.
fn ticket_pid(name: &str) -> Option<u64> {
    name.split('-').nth(1)?.parse().ok()
}

/// The start time stamped in a ticket's body; `None` when the ticket is
/// empty or unparsable (a create still landing), so the probe runs on pid
/// death and zombie state alone.
fn ticket_start(path: &Path) -> Option<u64> {
    let text = fs::read_to_string(path).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(text.lines().next()?).ok()?;
    parsed.get("start")?.as_u64()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_is_served_in_ticket_order_and_dead_tickets_are_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("graph.json.lock");
        let first = register(&lock).unwrap();
        let second = register(&lock).unwrap();
        assert!(am_head(&first), "earliest ticket is head");
        assert!(!am_head(&second), "later ticket waits behind the head");
        // A dead waiter that registered first must not block the queue: the
        // scan that answers am_head also prunes it. No real process can hold
        // i32::MAX as a pid, so the ticket reads as dead deterministically.
        let dead = queue_dir(&lock).join(format!("{:020}-{:07}-0000", 1u64, i32::MAX as u64));
        std::fs::File::create_new(&dead).unwrap();
        assert!(!am_head(&second), "second waits behind the live first");
        assert!(am_head(&first), "first is head once the dead one is gone");
        assert!(!dead.exists(), "the scan pruned the dead ticket");
    }

    /// Spawn a child that becomes a zombie: SIGKILLed but never waited. A
    /// zombie answers kill(pid,0) with 0, so pid_is_alive calls it alive
    /// while it can hold neither an fd nor a flock.
    fn zombie_child() -> std::process::Child {
        let child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("sleep spawns");
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGKILL) };
        child
    }

    #[test]
    fn zombie_ticket_is_pruned_and_serves_the_next_waiter() {
        let mut child = zombie_child();
        // The trap: the zombie still answers kill(pid,0), so the old
        // pid-death prune called its ticket live forever.
        assert!(
            crate::agent_lock::pid_is_alive(child.id() as u64),
            "the zombie still answers kill(pid,0)"
        );
        let start = crate::process_probe::process_bsd(child.id())
            .expect("zombie start readable")
            .0;
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("graph.json.lock");
        std::fs::create_dir_all(queue_dir(&lock)).unwrap();
        let dead = queue_dir(&lock).join(format!("{:020}-{:07}-0000", 1u64, child.id() as u64));
        std::fs::write(
            &dead,
            format!("{{\"pid\":{},\"start\":{start}}}", child.id()),
        )
        .unwrap();
        let second = register(&lock).unwrap();
        assert!(am_head(&second), "the zombie ticket pins no one");
        assert!(!dead.exists(), "the scan pruned the zombie ticket");
        let _ = child.wait();
    }

    #[test]
    fn recycled_pid_ticket_is_pruned_by_start_mismatch() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("sleep spawns");
        let real_start = crate::process_probe::process_bsd(child.id())
            .expect("live child start readable")
            .0;
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("graph.json.lock");
        std::fs::create_dir_all(queue_dir(&lock)).unwrap();
        let stale = queue_dir(&lock).join(format!("{:020}-{:07}-0000", 1u64, child.id() as u64));
        std::fs::write(
            &stale,
            format!(
                "{{\"pid\":{},\"start\":{}}}",
                child.id(),
                real_start.wrapping_add(1)
            ),
        )
        .unwrap();
        let second = register(&lock).unwrap();
        assert!(
            am_head(&second),
            "a recycled pid's stale ticket pins no one"
        );
        assert!(!stale.exists(), "the scan pruned the recycled-pid ticket");
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn live_holder_ticket_with_matching_start_is_never_pruned() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("sleep spawns");
        let start = crate::process_probe::process_bsd(child.id())
            .expect("live child start readable")
            .0;
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("graph.json.lock");
        std::fs::create_dir_all(queue_dir(&lock)).unwrap();
        let live = queue_dir(&lock).join(format!("{:020}-{:07}-0000", 1u64, child.id() as u64));
        std::fs::write(
            &live,
            format!("{{\"pid\":{},\"start\":{start}}}", child.id()),
        )
        .unwrap();
        let second = register(&lock).unwrap();
        assert!(
            !am_head(&second),
            "later waiter still waits behind a live head"
        );
        assert!(live.exists(), "the live holder ticket stays");
        let _ = child.kill();
        let _ = child.wait();
    }
}
