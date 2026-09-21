//! Arrival order for a contended claim key. ONE implementation, two callers:
//! `test_run::acquire_claim_blocking` in process, and `scripts/ci/preflight.sh`
//! over `fno-agents claim queue`. Ported from that script's bash queue, which
//! this module replaces.
//!
//! A bare claim poll loop gives every contender the same odds the instant the
//! holder releases, so a waiter that has queued for half an hour can be lapped
//! by one that arrived two minutes ago. The fix is the three properties the
//! bash queue stated before this module existed: tickets are allocated by
//! atomic mkdir (the same primitive as the lock), only the front `keys.len()`
//! waiters ever retry the real acquire, and a fresh arrival that finds waiters
//! already queued lines up behind them.
//!
//! Order lives on disk at `<claim_path>.queue.d/<NNNNNN>/holder`, one
//! directory per ticket, so a crashed contender cannot wedge it. Every waiter
//! reaps as it scans: a ticket is condemned when its pid is gone or its
//! recorded `create_time` no longer matches the live process (the recycled-pid
//! case). A ticket stamped on another machine cannot be pid-probed, so it is
//! skipped, never ordered. FIFO is fair, not optimal: a short run queues
//! behind a long one, and priority is deliberately out of scope.

use std::path::{Path, PathBuf};

/// One waiter's place in the queue: the ticket directory and its number.
/// Order is the ticket number, allocated above the highest surviving ticket.
pub struct Ticket {
    dir: PathBuf,
    seq: u64,
}

impl Ticket {
    /// The zero-padded directory name, which is also the wire form the
    /// `claim queue` verbs pass as `--ticket`.
    pub fn id(&self) -> String {
        format!("{:06}", self.seq)
    }
}

/// Where this waiter stands. `index` counts survivors ahead of it, so 0 is
/// the front; `total` is the survivor count including this one.
#[derive(Debug)]
pub struct Position {
    pub index: usize,
    pub total: usize,
}

/// The queue directory that orders a claim at `claim_path`: `<claim_path>.queue.d`.
pub fn queue_dir_for(claim_path: &Path) -> PathBuf {
    let mut s = claim_path.as_os_str().to_os_string();
    s.push(".queue.d");
    PathBuf::from(s)
}

/// Take a ticket at the back of the queue. The number is allocated ABOVE the
/// highest surviving ticket, never in the hole a dequeued front left:
/// restarting the scan at 1 would reissue 000001 while 000002 still waits,
/// putting a newcomer at the front. No clock in the counter: a clock step
/// backwards would let a newcomer sort ahead of a waiter.
pub fn enter(queue_dir: &Path) -> Result<Ticket, String> {
    todo!("claim_queue::enter")
}

/// This waiter's position, reaping dead and recycled tickets as the scan
/// walks. Fails when the ticket is no longer in the queue (its directory was
/// removed); the caller decides whether to re-enqueue or wait unordered.
pub fn position(t: &Ticket) -> Result<Position, String> {
    todo!("claim_queue::position")
}

/// Leave the queue. Idempotent: a ticket whose directory is already gone
/// (reaped by another waiter's scan) leaves quietly. Call on every return
/// path - the admitted path, the timeout path, and the admit/stop exits.
pub fn leave(t: Ticket) {
    todo!("claim_queue::leave")
}

/// Whether any ticket currently waits. A missing queue directory reads as no
/// waiters, never an error.
pub fn has_waiters(queue_dir: &Path) -> bool {
    todo!("claim_queue::has_waiters")
}

/// `fno-agents claim queue {enter,front,leave}` - the seam `preflight.sh`
/// calls instead of its old bash queue. Exit codes follow the claim-verb
/// convention: 0 success (or at the front), 1 not at the front, 2 usage or
/// error. `front` prints `position=N queued=M` (or `position=none` without a
/// ticket) so a caller can read the queue depth, and reaps as it scans.
pub fn run_queue(args: &[String]) -> i32 {
    todo!("claim_queue::run_queue")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn self_create_ms() -> i64 {
        crate::claims::process_create_time_ms(std::process::id() as i32)
            .expect("the test process must have a readable create time")
    }

    /// A pid that is certainly absent: spawn `true`, wait for the exit, reuse
    /// its number. Never a hard-coded pid, which could be live on some
    /// machine and make the test pass by accident.
    fn dead_pid() -> i32 {
        let mut child = Command::new("true")
            .spawn()
            .expect("spawn true for a dead pid");
        let pid = child.id() as i32;
        child.wait().expect("reap the true process");
        pid
    }

    fn ticket_at(dir: &Path, seq: u64, pid: i32, create_time: i64, machine: &str) -> PathBuf {
        let d = dir.join(format!("{seq:06}"));
        std::fs::create_dir_all(&d).expect("mkdir ticket");
        std::fs::write(
            d.join("holder"),
            format!(
                "pid={pid}\ncreate_time={create_time}\nhost=q-host\nmachine={machine}\nstarted=0\n"
            ),
        )
        .expect("stamp ticket");
        d
    }

    /// AC1-HP: with a live older ticket in the queue, a fresh contender's
    /// admission decision is "enqueue behind it", never "attempt acquire".
    /// Single-threaded and deterministic: the property under test is the
    /// ordering decision, not the scheduler.
    #[test]
    fn a_fresh_contender_queues_behind_a_live_older_ticket() {
        let dir = queue_dir_for(Path::new("/tmp/claim-q-lap.lock"));
        let _ = std::fs::remove_dir_all(&dir);
        ticket_at(
            &dir,
            1,
            std::process::id() as i32,
            self_create_ms(),
            &crate::claims::machine_id(),
        );
        let t = enter(&dir).expect("enter");
        let pos = position(&t).expect("position");
        assert!(
            pos.index >= 1,
            "a width-1 caller must not attempt acquire: {pos:?}"
        );
        assert_eq!(pos.total, 2, "both live tickets count: {pos:?}");
        leave(t);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC2-HP: tickets A, B, C enqueued in order; the front is A, then B,
    /// then C. The earliest arrival acquires first - the property itself,
    /// never a duration.
    #[test]
    fn the_front_is_the_earliest_arrival() {
        let dir = queue_dir_for(Path::new("/tmp/claim-q-order.lock"));
        let _ = std::fs::remove_dir_all(&dir);
        let a = enter(&dir).expect("enter a");
        let b = enter(&dir).expect("enter b");
        let c = enter(&dir).expect("enter c");
        assert_eq!(position(&a).expect("pos a").index, 0, "A is the front");
        leave(a);
        assert_eq!(position(&b).expect("pos b").index, 0, "B is next");
        leave(b);
        assert_eq!(position(&c).expect("pos c").index, 0, "C is last");
        leave(c);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC3-EDGE: a dead contender's ticket is reaped by the scan, and the
    /// next live ticket is the front. A dead contender does not wedge the
    /// queue.
    #[test]
    fn a_dead_ticket_is_reaped_and_never_blocks_the_queue() {
        let dir = queue_dir_for(Path::new("/tmp/claim-q-dead.lock"));
        let _ = std::fs::remove_dir_all(&dir);
        let corpse = ticket_at(&dir, 1, dead_pid(), 0, &crate::claims::machine_id());
        let t = enter(&dir).expect("enter");
        let pos = position(&t).expect("position");
        assert_eq!(pos.index, 0, "the live ticket is the front: {pos:?}");
        assert_eq!(pos.total, 1, "the dead ticket does not count: {pos:?}");
        assert!(!corpse.exists(), "the scan reaps the corpse it walks past");
        leave(t);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC4-EDGE: a ticket whose pid is alive but whose create time differs
    /// from the recorded one is condemned - the pid was recycled and its
    /// original owner cannot still be waiting.
    #[test]
    fn a_recycled_pid_ticket_is_condemned() {
        let dir = queue_dir_for(Path::new("/tmp/claim-q-recycle.lock"));
        let _ = std::fs::remove_dir_all(&dir);
        let stale = ticket_at(
            &dir,
            1,
            std::process::id() as i32,
            self_create_ms() + 123_456,
            &crate::claims::machine_id(),
        );
        let t = enter(&dir).expect("enter");
        let pos = position(&t).expect("position");
        assert_eq!(pos.index, 0, "the recycled ticket never blocks: {pos:?}");
        assert!(!stale.exists(), "the recycled ticket is removed");
        leave(t);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC5-HP: two contenders entering an empty queue in order get strictly
    /// increasing ticket numbers.
    #[test]
    fn ticket_numbers_increase_with_arrival() {
        let dir = queue_dir_for(Path::new("/tmp/claim-q-alloc.lock"));
        let _ = std::fs::remove_dir_all(&dir);
        let a = enter(&dir).expect("enter a");
        let b = enter(&dir).expect("enter b");
        assert!(
            b.seq > a.seq,
            "later arrival, higher ticket: {} then {}",
            a.id(),
            b.id()
        );
        leave(a);
        leave(b);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC6-EDGE: after the front leaves, a fresh arrival allocates ABOVE the
    /// highest surviving ticket, never in the hole (which would put it at the
    /// front of the queue).
    #[test]
    fn allocation_never_reuses_a_dequeued_fronts_number() {
        let dir = queue_dir_for(Path::new("/tmp/claim-q-hole.lock"));
        let _ = std::fs::remove_dir_all(&dir);
        let a = enter(&dir).expect("enter a");
        let b = enter(&dir).expect("enter b");
        leave(a);
        let c = enter(&dir).expect("enter c");
        assert!(
            c.seq > b.seq,
            "the hole is never reused: c={} must be above b={}",
            c.id(),
            b.id()
        );
        leave(b);
        leave(c);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC7-EDGE: a ticket stamped on another machine cannot be pid-probed,
    /// so it is skipped - it never blocks a local waiter, and it is never
    /// reaped (its owner may be live elsewhere).
    #[test]
    fn a_foreign_machine_ticket_is_skipped_never_ordered() {
        let dir = queue_dir_for(Path::new("/tmp/claim-q-foreign.lock"));
        let _ = std::fs::remove_dir_all(&dir);
        let foreign = ticket_at(
            &dir,
            1,
            std::process::id() as i32,
            self_create_ms(),
            "claim-q-test-foreign-machine",
        );
        let t = enter(&dir).expect("enter");
        let pos = position(&t).expect("position");
        assert_eq!(pos.index, 0, "the foreign ticket never blocks: {pos:?}");
        assert!(foreign.exists(), "an unprobeable ticket is never reaped");
        leave(t);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
