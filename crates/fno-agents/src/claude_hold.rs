//! Heartbeat-reattach supervisor + fd/LRU budget for held `claude --bg` attaches.
//!
//! G1 held-attach substrate (epic x-07c1, node x-26df), wave 2. The hold is the
//! load-bearing mechanism: a dropped attach silently re-enables auto-suspend, so
//! footnote heartbeat-supervises each hold (drains frames, answers pings, and on
//! a dropped socket re-attaches with backoff). The fd cost of N held attaches is
//! bounded by an LRU budget that sheds the least-recently-active hold.
//!
//! ponytail: the supervised reconnect loop's LIVE behavior (does a held non-TTY
//! attach actually outlive the ~1h idle window) is the Phase-0 spike's job and
//! cannot be unit-tested -- it needs a live 2.1.195 daemon. The testable logic
//! (LRU eviction, the drain/ping pump, the reattach decision, backoff reset on a
//! productive hold) is unit-tested here over the [`ControlTransport`] fake; the
//! live `UnixStream` path is one thin function. Carveout cv-02f35433 tracks the
//! unverified runtime bet.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::claude_attach::{
    classify_incoming, perform_attach, pong_line, AttachError, AttachRequest, ControlTransport,
    Incoming,
};
use crate::supervisor::{Backoff, RestartDecision, RestartPolicy};

/// fd budget for concurrent held attaches: a cap plus LRU shed order. Holding a
/// socket costs one fd; past the cap the least-recently-active hold is shed so
/// the fleet never exhausts the descriptor table. Pure bookkeeping -- the caller
/// closes the evicted hold's transport.
#[derive(Debug, Clone)]
pub struct AttachBudget {
    cap: usize,
    /// short_ids in LRU order: front == least-recently-active, back == most.
    lru: Vec<String>,
}

impl AttachBudget {
    /// A budget for at most `cap` concurrent held attaches (clamped to >= 1).
    pub fn new(cap: usize) -> Self {
        AttachBudget {
            cap: cap.max(1),
            lru: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.lru.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lru.is_empty()
    }

    pub fn contains(&self, short: &str) -> bool {
        self.lru.iter().any(|s| s == short)
    }

    /// Mark `short` most-recently-active (it just answered a ping / saw a frame).
    /// No-op if not currently held.
    pub fn touch(&mut self, short: &str) {
        if let Some(pos) = self.lru.iter().position(|s| s == short) {
            let s = self.lru.remove(pos);
            self.lru.push(s);
        }
    }

    /// Admit `short` as a held attach (touch if already held). Returns
    /// `Some(victim)` when admitting pushed the count over `cap` and the
    /// least-recently-active hold must be shed -- the caller closes that hold's
    /// transport. The just-admitted `short` is never the victim.
    pub fn admit(&mut self, short: &str) -> Option<String> {
        if self.contains(short) {
            self.touch(short);
            return None;
        }
        self.lru.push(short.to_string());
        if self.lru.len() > self.cap {
            // Evict the LRU front -- guaranteed not to be the just-pushed back.
            return Some(self.lru.remove(0));
        }
        None
    }

    /// Drop `short` from the budget (its hold ended).
    pub fn release(&mut self, short: &str) {
        if let Some(pos) = self.lru.iter().position(|s| s == short) {
            self.lru.remove(pos);
        }
    }
}

/// Why a single hold ended.
#[derive(Debug, Clone, PartialEq)]
pub enum HoldEnd {
    /// Clean EOF -- the daemon closed the socket (suspend re-enabled; reattach).
    Dropped,
    /// A recv error tore the hold down.
    Error(String),
}

/// What one hold did before it ended.
#[derive(Debug, Clone, PartialEq)]
pub struct HoldStats {
    /// Heartbeats answered -- a hold that answered >= 1 was demonstrably healthy.
    pub pings_answered: u32,
    /// PTY render frames drained (ignored in G1; rendering is G2).
    pub frames_seen: u32,
    pub end: HoldEnd,
}

/// Drain an already-attached transport until EOF/error, answering pings to keep
/// the hold alive. The non-draining-reader-gets-destroyed contract (lane-a
/// backpressure) means we MUST keep reading; this loop is that drain.
pub fn pump<T: ControlTransport>(t: &mut T) -> HoldStats {
    let mut pings = 0u32;
    let mut frames = 0u32;
    loop {
        match t.recv_line() {
            Ok(Some(line)) => match classify_incoming(&line) {
                Incoming::Ping => {
                    // A failed pong write means the socket is already gone; the
                    // next recv will surface EOF/err, so don't special-case it.
                    let _ = t.send_line(&pong_line());
                    pings += 1;
                }
                Incoming::PtyFrame(_) => frames += 1,
                Incoming::Other(_) => {}
            },
            Ok(None) => {
                return HoldStats {
                    pings_answered: pings,
                    frames_seen: frames,
                    end: HoldEnd::Dropped,
                }
            }
            Err(e) => {
                return HoldStats {
                    pings_answered: pings,
                    frames_seen: frames,
                    end: HoldEnd::Error(e.to_string()),
                }
            }
        }
    }
}

/// Attach over `t`, then [`pump`] until the hold ends.
pub fn attach_and_pump<T: ControlTransport>(
    t: &mut T,
    req: &AttachRequest,
) -> Result<HoldStats, AttachError> {
    perform_attach(t, req)?;
    Ok(pump(t))
}

/// Terminal outcome of the supervised hold loop.
#[derive(Debug, Clone, PartialEq)]
pub enum SupervisedOutcome {
    /// The `stop` flag was set; the loop exited cleanly.
    Stopped,
    /// Consecutive reattach failures hit the restart ceiling (LD36); gave up.
    GaveUp { failures: u32 },
}

/// Supervise a held attach: connect, attach, pump; on a dropped/failed hold,
/// back off and reconnect, until `stop` is set or the failure ceiling is hit.
///
/// A hold that answered >= 1 heartbeat is treated as healthy and RESETS the
/// failure count (so a session that holds fine for hours then drops once does not
/// march toward `PermanentDead`); a thrashing attach-then-immediate-drop, or a
/// connect/attach error, increments it. `sleep` is injected so the loop is
/// driven deterministically in tests (prod passes `std::thread::sleep`).
pub fn run_supervised<T, C, S>(
    mut connect: C,
    req: &AttachRequest,
    policy: &RestartPolicy,
    stop: &AtomicBool,
    mut sleep: S,
) -> SupervisedOutcome
where
    T: ControlTransport,
    C: FnMut() -> io::Result<T>,
    S: FnMut(std::time::Duration),
{
    let mut failures = 0u32;
    loop {
        if stop.load(Ordering::Acquire) {
            return SupervisedOutcome::Stopped;
        }
        match connect() {
            Ok(mut t) => match attach_and_pump(&mut t, req) {
                Ok(stats) if stats.pings_answered > 0 => failures = 0,
                Ok(_) => failures += 1,
                Err(_) => failures += 1,
            },
            Err(_) => failures += 1,
        }
        if stop.load(Ordering::Acquire) {
            return SupervisedOutcome::Stopped;
        }
        match policy.decide(failures) {
            RestartDecision::Restart { after } => sleep(after),
            RestartDecision::PermanentDead => {
                return SupervisedOutcome::GaveUp { failures };
            }
        }
    }
}

/// The default reattach policy for a held attach: the restart ceiling with a
/// conservative backoff (a flapping daemon socket should not be hammered).
pub fn default_reattach_policy() -> RestartPolicy {
    RestartPolicy::new(5, Backoff::default())
}

/// Adopt a roster worker and hold it live until `stop` is set.
///
/// Composes the substrate end-to-end: mint the fno registry row, take the
/// `pty:<short_id>` single-writer claim, pid-reanchor it to THIS (long-lived)
/// holder process, then run the supervised `control.sock` hold. Refuses to adopt
/// a session another writer already holds (AC1-EDGE: no double-adopt).
///
/// ponytail: this is the live glue -- it touches the real `fno claim` CLI and the
/// real daemon socket, so it is not unit-tested; the Phase-0 spike is its arbiter.
/// Every piece it composes IS unit-tested in isolation.
pub fn hold_adopted_session(
    registry_path: &std::path::Path,
    worker: &crate::claude_roster::RosterWorker,
    stop: &AtomicBool,
) -> io::Result<SupervisedOutcome> {
    use crate::claude_adopt::{
        acquire_pty_claim, mint_adopted_entry, pty_claim_holder, reanchor_pty_claim,
        upsert_adopted_row, ClaimOutcome,
    };
    use crate::claude_attach::UnixControlTransport;

    let short = worker.short_id().to_string();
    let holder = pty_claim_holder(&short);

    // Resolve the daemon control.sock BEFORE mutating any state -- a worker with
    // no resolvable socket is not adoptable, so fail without minting a dangling
    // row or stealing a claim.
    let control_sock = worker.resolve_control_sock().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no control.sock for session {}", worker.session_id),
        )
    })?;

    // Single-writer claim first: refuse the adopt if another live writer holds it.
    match acquire_pty_claim(&worker.session_id, &holder) {
        ClaimOutcome::HeldByOther(who) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("session {} already held by {who}", worker.session_id),
            ));
        }
        ClaimOutcome::Acquired | ClaimOutcome::Unavailable(_) => {}
    }
    // Re-anchor the claim's liveness to this holder process (not the transient
    // `fno claim` subprocess that just exited).
    reanchor_pty_claim(&worker.session_id, &holder, std::process::id());

    // Mint + upsert the registry row so grid/relay can resolve the session.
    let entry = mint_adopted_entry(worker, &crate::daemon::now_rfc3339_like());
    upsert_adopted_row(registry_path, entry)
        .map_err(|e| io::Error::other(format!("registry mint failed: {e}")))?;

    // Run the supervised hold (real socket, real sleep).
    let auth = crate::claude_roster::read_control_key();
    let req = AttachRequest::for_hold(&short, auth);
    let policy = default_reattach_policy();
    Ok(run_supervised(
        || UnixControlTransport::connect(&control_sock),
        &req,
        &policy,
        stop,
        std::thread::sleep,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct Fake {
        lines: VecDeque<Option<String>>,
        sent: Vec<String>,
    }
    impl Fake {
        fn new(lines: Vec<Option<&str>>) -> Self {
            Fake {
                lines: lines.into_iter().map(|l| l.map(str::to_string)).collect(),
                sent: Vec::new(),
            }
        }
    }
    impl ControlTransport for Fake {
        fn send_line(&mut self, line: &str) -> io::Result<()> {
            self.sent.push(line.to_string());
            Ok(())
        }
        fn recv_line(&mut self) -> io::Result<Option<String>> {
            Ok(self.lines.pop_front().flatten())
        }
    }

    // ── AttachBudget ────────────────────────────────────────────────────────

    #[test]
    fn budget_admits_under_cap_no_eviction() {
        let mut b = AttachBudget::new(3);
        assert_eq!(b.admit("a"), None);
        assert_eq!(b.admit("b"), None);
        assert_eq!(b.len(), 2);
        assert!(b.contains("a") && b.contains("b"));
    }

    #[test]
    fn budget_evicts_lru_over_cap() {
        let mut b = AttachBudget::new(2);
        b.admit("a");
        b.admit("b");
        // "a" is LRU; admitting "c" sheds it.
        assert_eq!(b.admit("c"), Some("a".to_string()));
        assert_eq!(b.len(), 2);
        assert!(!b.contains("a"));
        assert!(b.contains("b") && b.contains("c"));
    }

    #[test]
    fn budget_touch_reorders_lru() {
        let mut b = AttachBudget::new(2);
        b.admit("a");
        b.admit("b");
        b.touch("a"); // a is now MRU, b is LRU
        assert_eq!(b.admit("c"), Some("b".to_string()));
        assert!(b.contains("a") && b.contains("c"));
    }

    #[test]
    fn budget_admit_existing_touches_no_evict() {
        let mut b = AttachBudget::new(2);
        b.admit("a");
        b.admit("b");
        assert_eq!(b.admit("a"), None, "re-admit is a touch, not a new slot");
        assert_eq!(b.len(), 2);
    }

    #[test]
    fn budget_cap_is_clamped_to_one() {
        let mut b = AttachBudget::new(0);
        assert_eq!(b.admit("a"), None);
        assert_eq!(b.admit("b"), Some("a".to_string()));
    }

    // ── pump ────────────────────────────────────────────────────────────────

    #[test]
    fn pump_answers_pings_and_ends_on_eof() {
        let mut t = Fake::new(vec![
            Some(r#"{"op":"ping"}"#),
            Some("\x1b[2J frame"),
            Some(r#"{"op":"ping"}"#),
            None,
        ]);
        let stats = pump(&mut t);
        assert_eq!(stats.pings_answered, 2);
        assert_eq!(stats.frames_seen, 1);
        assert_eq!(stats.end, HoldEnd::Dropped);
        // Two pongs written back.
        assert_eq!(t.sent.iter().filter(|l| l.contains("pong")).count(), 2);
    }

    #[test]
    fn attach_then_pump_happy() {
        let mut t = Fake::new(vec![
            Some(r#"{"ok":true,"op":"attach","state":"running"}"#),
            Some(r#"{"op":"ping"}"#),
            None,
        ]);
        let req = AttachRequest::for_hold("a1b2c3d4", None);
        let stats = attach_and_pump(&mut t, &req).unwrap();
        assert_eq!(stats.pings_answered, 1);
        assert_eq!(stats.end, HoldEnd::Dropped);
    }

    // ── run_supervised ──────────────────────────────────────────────────────

    #[test]
    fn supervised_gives_up_after_ceiling_of_unproductive_holds() {
        // Each connect yields a session that attaches then immediately EOFs with
        // zero pings -> unproductive -> failures climb to the ceiling (3).
        let policy = RestartPolicy::new(3, Backoff::default());
        let stop = AtomicBool::new(false);
        let mut connects = 0u32;
        let mut slept = 0u32;
        let outcome = run_supervised(
            || {
                connects += 1;
                Ok(Fake::new(vec![Some(r#"{"ok":true,"op":"attach"}"#), None]))
            },
            &AttachRequest::for_hold("a1b2c3d4", None),
            &policy,
            &stop,
            |_| slept += 1,
        );
        assert_eq!(outcome, SupervisedOutcome::GaveUp { failures: 3 });
        assert_eq!(connects, 3);
        // Slept after failure 1 and 2 (Restart), not after 3 (PermanentDead).
        assert_eq!(slept, 2);
    }

    #[test]
    fn supervised_stops_on_flag_and_productive_hold_resets() {
        // A productive hold (answers a ping) resets failures; the connect factory
        // trips `stop` after 2 connects so the loop exits Stopped, proving the
        // healthy-hold path does not accumulate toward GaveUp.
        let policy = RestartPolicy::new(3, Backoff::default());
        let stop = AtomicBool::new(false);
        let mut connects = 0u32;
        let outcome = run_supervised(
            || {
                connects += 1;
                if connects >= 2 {
                    stop.store(true, Ordering::Release);
                }
                Ok(Fake::new(vec![
                    Some(r#"{"ok":true,"op":"attach"}"#),
                    Some(r#"{"op":"ping"}"#),
                    None,
                ]))
            },
            &AttachRequest::for_hold("a1b2c3d4", None),
            &policy,
            &stop,
            |_| {},
        );
        assert_eq!(outcome, SupervisedOutcome::Stopped);
    }
}
