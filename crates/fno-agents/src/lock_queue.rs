//! FIFO ordering for [`crate::graph_store::BoundedLock`] waiters.
//!
//! flock(2) wakes an arbitrary waiter, so under CPU starvation a poller can
//! lose the wakeup race repeatedly and time out while the lock is free (a
//! waiter is served, not timed out). Tickets make the order explicit: each
//! waiter writes one ticket file beside the lock and only the oldest live
//! ticket may run `try_lock`. flock stays the source of truth - a ticketless
//! writer (an older binary mid-rollout) can still take the lock out of
//! order - so this is fairness, not correctness. A dead waiter's ticket is
//! pruned by any scan (the same dead-pid contract the lock stamp's holder
//! summary uses); a stalled head is bounded by its own deadline, and
//! withdrawing it serves the next ticket.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::agent_lock::pid_is_alive;

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
/// tickets whose parseable pid is dead; a ticket that does not parse (a
/// create still landing) counts as live, never as absent.
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
            if !pid_is_alive(pid) {
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

/// The pid encoded in a ticket name's middle field.
fn ticket_pid(name: &str) -> Option<u64> {
    name.split('-').nth(1)?.parse().ok()
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
        assert!(
            !am_head(&second),
            "second still waits behind the live first ticket"
        );
        assert!(
            am_head(&first),
            "first is head once the dead ticket is gone"
        );
        assert!(!dead.exists(), "the scan pruned the dead ticket");
    }
}
