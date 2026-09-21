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

/// Take a ticket at the back of the queue, stamped with this process. The
/// number is allocated ABOVE the highest surviving ticket, never in the hole
/// a dequeued front left: restarting the scan at 1 would reissue 000001
/// while 000002 still waits, putting a newcomer at the front. No clock in
/// the counter: a clock step backwards would let a newcomer sort ahead of a
/// waiter.
pub fn enter(queue_dir: &Path) -> Result<Ticket, String> {
    enter_as(queue_dir, std::process::id())
}

/// [`enter`] for a ticket naming ANOTHER live process: the verb seam stamps
/// the caller's pid, because a one-shot verb process is dead by its own next
/// scan and every ticket it stamped would reap itself.
pub fn enter_as(queue_dir: &Path, pid: u32) -> Result<Ticket, String> {
    std::fs::create_dir_all(queue_dir)
        .map_err(|e| format!("cannot create queue dir {}: {e}", queue_dir.display()))?;
    let mut n = highest_ticket(queue_dir)?.map_or(1, |h| h + 1);
    loop {
        let candidate = queue_dir.join(format!("{n:06}"));
        match std::fs::create_dir(&candidate) {
            Ok(()) => {
                // create_time is recorded only when the probe says Created:
                // an unreadable birth time (foreign-uid target) leaves the
                // field off, and the scan treats an unprobeable live pid as
                // alive, never condemning on a guess.
                let create_line = match crate::claims::probe_pid(pid as i32) {
                    crate::claims::PidProbe::Created(ct) => format!("create_time={ct}\n"),
                    _ => String::new(),
                };
                let stamp = format!(
                    "pid={pid}\n{create_line}host={}\nmachine={}\nstarted={}\n",
                    crate::claims::hostname(),
                    crate::claims::machine_id(),
                    crate::claims::now_ms(),
                );
                return match std::fs::write(candidate.join("holder"), stamp) {
                    Ok(()) => Ok(Ticket {
                        dir: candidate,
                        seq: n,
                    }),
                    // A ticket without a stamp is a corpse the next scan would
                    // reap; drop it now and refuse instead.
                    Err(e) => {
                        let _ = std::fs::remove_dir_all(&candidate);
                        Err(format!(
                            "cannot stamp queue ticket at {}: {e}",
                            candidate.display()
                        ))
                    }
                };
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
            Err(e) => {
                return Err(format!(
                    "cannot enqueue ticket in {}: {e}",
                    queue_dir.display()
                ))
            }
        }
    }
}

/// This waiter's position, reaping dead and recycled tickets as the scan
/// walks. Fails when the ticket is no longer in the queue (its directory was
/// removed); the caller decides whether to re-enqueue or wait unordered.
pub fn position(t: &Ticket) -> Result<Position, String> {
    let queue_dir = t
        .dir
        .parent()
        .ok_or_else(|| format!("queue ticket {} has no queue dir", t.id()))?;
    let (survivors, found_self) = scan(queue_dir, Some(t.seq))?;
    if !found_self {
        return Err(format!(
            "queue ticket {} is gone from {}: it was removed; re-enqueue before waiting",
            t.id(),
            queue_dir.display()
        ));
    }
    let index = survivors.iter().position(|s| *s == t.seq).unwrap_or(0);
    Ok(Position {
        index,
        total: survivors.len(),
    })
}

/// Leave the queue. Idempotent: a ticket whose directory is already gone
/// (reaped by another waiter's scan) leaves quietly. Call on every return
/// path - the admitted path, the timeout path, and the admit/stop exits.
pub fn leave(t: Ticket) {
    let _ = std::fs::remove_dir_all(&t.dir);
}

/// Whether any ticket currently waits. A missing queue directory reads as no
/// waiters, never an error.
pub fn has_waiters(queue_dir: &Path) -> bool {
    highest_ticket(queue_dir).map_or(false, |h| h.is_some())
}

/// `fno-agents claim queue {enter,front,leave}` - the seam `preflight.sh`
/// calls instead of its old bash queue. Exit codes follow the claim-verb
/// convention: 0 success (or at the front), 1 not at the front, 2 usage or
/// error. `front` prints `position=N queued=M` (or `position=none` without a
/// ticket) so a caller can read the queue depth, and reaps as it scans.
pub fn run_queue(args: &[String]) -> i32 {
    let Some(op) = args.first().map(String::as_str) else {
        eprintln!("fno-agents: claim queue requires an operation: enter|front|leave");
        return 2;
    };
    let mut dir: Option<PathBuf> = None;
    let mut ticket: Option<u64> = None;
    let mut pid: Option<u32> = None;
    let mut it = args[1..].iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dir" => match it.next() {
                Some(v) => dir = Some(PathBuf::from(v)),
                None => {
                    eprintln!("fno-agents: claim queue {op}: --dir needs a value");
                    return 2;
                }
            },
            "--pid" => match it.next().map(|v| v.parse::<u32>()) {
                Some(Ok(n)) => pid = Some(n),
                Some(Err(_)) => {
                    eprintln!("fno-agents: claim queue {op}: --pid: not a pid");
                    return 2;
                }
                None => {
                    eprintln!("fno-agents: claim queue {op}: --pid needs a value");
                    return 2;
                }
            },
            "--ticket" => match it.next().map(|v| v.parse::<u64>()) {
                Some(Ok(n)) => ticket = Some(n),
                Some(Err(_)) => {
                    eprintln!("fno-agents: claim queue {op}: --ticket: not a ticket number");
                    return 2;
                }
                None => {
                    eprintln!("fno-agents: claim queue {op}: --ticket needs a value");
                    return 2;
                }
            },
            other => {
                eprintln!("fno-agents: claim queue {op}: unrecognized argument: {other}");
                return 2;
            }
        }
    }
    let Some(dir) = dir else {
        eprintln!("fno-agents: claim queue {op} requires --dir <queue-dir>");
        return 2;
    };
    match op {
        "enter" => match match pid {
            Some(p) => enter_as(&dir, p),
            None => enter(&dir),
        } {
            Ok(t) => {
                println!("{}", t.id());
                0
            }
            Err(e) => {
                eprintln!("fno-agents: claim queue enter --pid: {e}");
                2
            }
        },
        "front" => {
            let Some(seq) = ticket else {
                // No ticket: a probe read for the queue depth. It still reaps,
                // and it never answers "front" (exit 1) because its caller is
                // not in the queue.
                return match scan(&dir, None) {
                    Ok((survivors, _)) => {
                        println!("position=none queued={}", survivors.len());
                        1
                    }
                    Err(e) => {
                        eprintln!("fno-agents: claim queue front: {e}");
                        2
                    }
                };
            };
            let t = Ticket {
                dir: dir.join(format!("{seq:06}")),
                seq,
            };
            match position(&t) {
                Ok(pos) => {
                    println!("position={} queued={}", pos.index, pos.total);
                    if pos.index == 0 {
                        0
                    } else {
                        1
                    }
                }
                Err(e) => {
                    eprintln!("fno-agents: claim queue front: {e}");
                    2
                }
            }
        }
        "leave" => {
            let Some(seq) = ticket else {
                eprintln!("fno-agents: claim queue leave requires --ticket <id>");
                return 2;
            };
            leave(Ticket {
                dir: dir.join(format!("{seq:06}")),
                seq,
            });
            0
        }
        other => {
            eprintln!("fno-agents: claim queue: unknown operation: {other} (enter|front|leave)");
            2
        }
    }
}

/// The highest surviving ticket number, or `None` for an empty or missing
/// queue. Non-numeric names are ignored, never errors.
fn highest_ticket(queue_dir: &Path) -> Result<Option<u64>, String> {
    Ok(scan_names(queue_dir)?.into_iter().max())
}

fn scan_names(queue_dir: &Path) -> Result<Vec<u64>, String> {
    let mut names: Vec<u64> = Vec::new();
    match std::fs::read_dir(queue_dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry
                    .map_err(|e| format!("cannot read queue dir {}: {e}", queue_dir.display()))?;
                let name = entry.file_name();
                let Some(s) = name.to_str() else { continue };
                if let Ok(n) = s.parse::<u64>() {
                    names.push(n);
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(format!(
                "cannot read queue dir {}: {e}",
                queue_dir.display()
            ))
        }
    }
    Ok(names)
}

struct Stamp {
    pid: Option<i32>,
    create_time: Option<i64>,
    /// bash-era stamps carry an ISO-8601 `started=` instead of `create_time`.
    /// Held as epoch seconds for the recycled-pid compare.
    started_s: Option<i64>,
    machine: String,
    host: String,
}

/// A ticket whose stamp cannot be read at all reads as a live waiter, never a
/// corpse: condemning on a guess laps a waiter we cannot prove is gone.
fn read_stamp(dir: &Path) -> Option<Stamp> {
    let text = std::fs::read_to_string(dir.join("holder")).ok()?;
    let (mut pid, mut create_time, mut started_s) = (None, None, None);
    let (mut machine, mut host) = (String::new(), String::new());
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("pid=") {
            pid = v.parse().ok();
        } else if let Some(v) = line.strip_prefix("create_time=") {
            create_time = v.parse().ok();
        } else if let Some(v) = line.strip_prefix("started=") {
            started_s = iso8601_utc_to_epoch_s(v);
        } else if let Some(v) = line.strip_prefix("machine=") {
            machine = v.to_string();
        } else if let Some(v) = line.strip_prefix("host=") {
            host = v.to_string();
        }
    }
    Some(Stamp {
        pid,
        create_time,
        started_s,
        machine,
        host,
    })
}

/// `YYYY-MM-DDTHH:MM:SSZ` to epoch seconds, the stamp format the deleted bash
/// queue wrote. Days-from-civil (Hinnant); no calendar dependency. Unparsable
/// input returns `None` and the caller keeps the ticket - never condemn on a
/// guess.
fn iso8601_utc_to_epoch_s(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 20
        || b[4] != b'-'
        || b[7] != b'-'
        || (b[10] != b'T' && b[10] != b' ')
        || b[13] != b':'
        || b[16] != b':'
        || b[19] != b'Z'
    {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let y = if mo <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + h * 3600 + mi * 60 + sec)
}

/// A ticket stamped on a machine we cannot pid-probe is skipped: it never
/// orders a local waiter and it is never reaped (its owner may be live
/// elsewhere). Machine identity only: a legacy stamp with no `machine=`
/// field cannot differ, so it goes to the pid probe like bash's reaper did.
/// Hostnames move under a roaming laptop and the bash-era fixtures write
/// `host=x`, so a host compare would misclassify a live local ticket.
fn stamp_is_foreign(stamp: &Stamp) -> bool {
    let mine = crate::claims::machine_id();
    !stamp.machine.is_empty() && !mine.is_empty() && stamp.machine != mine
}

/// Dead = the pid is gone, or was recycled (a live but different incarnation
/// now owns the number; the ticket's author cannot still be it). A stamp
/// without a parsable pid survives, like bash's ticket_is_dead. A pid whose
/// birth time is unreadable (foreign-uid) is ALIVE-but-unprobeable, and an
/// unprobeable ticket is kept, never condemned - it is ordered or skipped by
/// machine identity, not killed on a guess.
fn ticket_is_dead(stamp: &Stamp) -> bool {
    let Some(pid) = stamp.pid else {
        return false;
    };
    match crate::claims::probe_pid(pid) {
        crate::claims::PidProbe::Absent => true,
        crate::claims::PidProbe::Refused => false,
        crate::claims::PidProbe::Created(actual) => match stamp.create_time {
            Some(recorded) => actual != recorded,
            // bash-era stamp: recycled when the stamp predates the live
            // process's birth by more than the minute of slop bash's
            // holder_pid_recycled allowed.
            None => match stamp.started_s {
                Some(started) => started < actual / 1000 - 60,
                None => false,
            },
        },
    }
}

/// Survivor numbers in arrival order, with whether `self_seq` is among them.
/// Dead and recycled tickets are reaped here as the scan walks, so a crashed
/// waiter never blocks the queue and no sweep lane exists.
fn scan(queue_dir: &Path, self_seq: Option<u64>) -> Result<(Vec<u64>, bool), String> {
    let mut names = scan_names(queue_dir)?;
    names.sort_unstable();
    let mut survivors: Vec<u64> = Vec::with_capacity(names.len());
    let mut found_self = false;
    for seq in names {
        let dir = queue_dir.join(format!("{seq:06}"));
        let stamp = read_stamp(&dir);
        if stamp.as_ref().is_some_and(stamp_is_foreign) {
            continue;
        }
        if stamp.as_ref().is_some_and(ticket_is_dead) {
            let _ = std::fs::remove_dir_all(&dir);
            continue;
        }
        found_self |= self_seq == Some(seq);
        survivors.push(seq);
    }
    Ok((survivors, found_self))
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

    /// A scan that condemns our own ticket reports GONE, never "front of an
    /// empty queue": found_self is decided by the survivor walk, not by name
    /// match before the reaper. The one-shot-pid verb hazard this regression
    /// pins: a ticket stamped with a now-dead pid must read as gone (exit 2),
    /// so the caller re-enqueues instead of claiming an unearned front.
    #[test]
    fn a_condemned_self_ticket_reads_as_gone_not_front() {
        let dir = queue_dir_for(Path::new("/tmp/claim-q-gone.lock"));
        let _ = std::fs::remove_dir_all(&dir);
        let corpse = ticket_at(&dir, 1, dead_pid(), 0, &crate::claims::machine_id());
        let t = Ticket {
            dir: corpse.clone(),
            seq: 1,
        };
        assert!(
            position(&t).is_err(),
            "a condemned self ticket must error, never read as front"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A bash-era stamp (ISO `started=`, no `create_time`) whose stamp
    /// predates the live process's birth by more than the recycle slop is
    /// condemned - the phantom contract the deleted bash queue's tests pin.
    #[test]
    fn a_bash_era_phantom_stamp_is_condemned() {
        let dir = queue_dir_for(Path::new("/tmp/claim-q-phantom.lock"));
        let _ = std::fs::remove_dir_all(&dir);
        let phantom = dir.join("000001");
        std::fs::create_dir_all(&phantom).expect("mkdir phantom");
        std::fs::write(
            phantom.join("holder"),
            format!(
                "pid={}\nstarted=2020-01-01T00:00:00Z\nhost=q-host\n",
                std::process::id()
            ),
        )
        .expect("stamp phantom");
        let t = enter(&dir).expect("enter");
        let pos = position(&t).expect("position");
        assert_eq!(pos.index, 0, "the phantom never blocks: {pos:?}");
        assert!(!phantom.exists(), "the phantom is reaped");
        leave(t);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A bash-era stamp that is NOT older than the live process survives:
    /// the stamp plausibly names this incarnation, so the pid arm keeps it.
    #[test]
    fn a_fresh_bash_era_stamp_survives() {
        let dir = queue_dir_for(Path::new("/tmp/claim-q-legacy.lock"));
        let _ = std::fs::remove_dir_all(&dir);
        let fresh = dir.join("000001");
        std::fs::create_dir_all(&fresh).expect("mkdir legacy");
        std::fs::write(
            fresh.join("holder"),
            format!(
                "pid={}\nstarted=2030-01-01T00:00:00Z\nhost=q-host\n",
                std::process::id()
            ),
        )
        .expect("stamp legacy");
        let t = enter(&dir).expect("enter");
        let pos = position(&t).expect("position");
        assert_eq!(pos.total, 2, "the fresh legacy stamp survives: {pos:?}");
        assert!(fresh.exists(), "a plausibly-live legacy ticket is kept");
        leave(t);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
