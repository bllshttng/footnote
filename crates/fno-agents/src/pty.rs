//! PTY spawn + bounded-ring output drainer (design module `pty.rs`).
//!
//! Per Wave 0's Outcome B, this is **worker-side**: a worker process owns the
//! PTY master for an agent's whole lifetime so the child survives daemon
//! restarts. [`PtySession`] spawns a child on a fresh PTY, owns the master, and
//! runs a drainer thread that copies master output into a [`BoundedRing`].
//!
//! LD31: the drainer is always running with a bounded ring (1MB default,
//! `config.pty.output_ring_bytes`). On overflow it drops the OLDEST bytes and
//! accounts them in [`BoundedRing::dropped_bytes`]; the daemon (Wave 3) turns
//! that counter into rate-limited `pty_output_dropped` events. Dropping oldest
//! (not blocking) is what prevents the child from deadlocking on a full kernel
//! pipe buffer.
//!
//! Domain Pitfall (deferred to Wave 3): the production daemon decouples the
//! drainer (PTY read -> ring, never touches disk) from a separate timeline
//! writer task (ring -> timeline.jsonl). Wave 1 ships the drainer + ring; the
//! timeline writer is a Wave 3 tokio task that consumes [`PtySession::snapshot`]
//! / a future incremental cursor.

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How long `Drop` waits for the drainer to terminate before detaching it.
const DRAINER_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// LD31 default ring capacity.
pub const DEFAULT_OUTPUT_RING_BYTES: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    #[error("failed to open pty: {0}")]
    OpenPty(String),
    #[error("failed to spawn child: {0}")]
    Spawn(String),
    #[error("failed to obtain pty writer: {0}")]
    Writer(String),
    #[error("failed to obtain pty reader: {0}")]
    Reader(String),
    #[error("pty write failed: {0}")]
    Write(std::io::Error),
    #[error("pty resize failed: {0}")]
    Resize(String),
    #[error("child wait failed: {0}")]
    Wait(String),
    #[error("child kill failed: {0}")]
    Kill(String),
}

/// A fixed-capacity byte ring. On overflow the oldest bytes are dropped (never
/// blocks the writer) and counted so the consumer can surface backpressure.
#[derive(Debug)]
pub struct BoundedRing {
    buf: VecDeque<u8>,
    capacity: usize,
    dropped: u64,
}

impl BoundedRing {
    pub fn new(capacity: usize) -> Self {
        BoundedRing {
            buf: VecDeque::new(),
            capacity: capacity.max(1),
            dropped: 0,
        }
    }

    /// Append `data`, dropping the oldest bytes if it would exceed capacity.
    pub fn extend(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        if data.len() >= self.capacity {
            // The incoming chunk alone fills the ring: everything currently
            // buffered plus the chunk's leading excess is dropped.
            let keep_from = data.len() - self.capacity;
            self.dropped += self.buf.len() as u64 + keep_from as u64;
            self.buf.clear();
            self.buf.extend(&data[keep_from..]);
            return;
        }
        let overflow = (self.buf.len() + data.len()).saturating_sub(self.capacity);
        for _ in 0..overflow {
            self.buf.pop_front();
        }
        self.dropped += overflow as u64;
        self.buf.extend(data);
    }

    /// Copy the current contents out (oldest-to-newest).
    pub fn snapshot(&self) -> Vec<u8> {
        self.buf.iter().copied().collect()
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Total bytes dropped due to overflow over this ring's lifetime.
    pub fn dropped_bytes(&self) -> u64 {
        self.dropped
    }

    /// Total bytes ever written to the ring (dropped + currently buffered).
    /// This is a monotonic cursor: a reader that remembers the `next` value
    /// from [`BoundedRing::read_since`] can ask for everything appended since,
    /// without the fragile two-snapshot diff a sliding window would otherwise
    /// require. Drive output streaming (Wave 4) rides on this.
    pub fn total_written(&self) -> u64 {
        self.dropped + self.buf.len() as u64
    }

    /// Return everything appended after the absolute byte offset `cursor`.
    ///
    /// `next` is the offset to pass on the following call. `gap` is true when
    /// `cursor` pointed at bytes the ring has since dropped (overflow), so the
    /// reader knows its stream skipped ahead and can surface a drop notice
    /// rather than silently splicing non-contiguous output. A `cursor` past the
    /// current tail (a reader ahead of the writer, or a stale-but-future value)
    /// yields no bytes and `next == cursor.max(total)`.
    pub fn read_since(&self, cursor: u64) -> ReadSince {
        let total = self.total_written();
        if cursor >= total {
            // Reader is at or ahead of the tail: nothing new. Never rewind the
            // caller's cursor below where they already were.
            return ReadSince {
                bytes: Vec::new(),
                next: cursor.max(total),
                gap: false,
            };
        }
        // cursor < total, so there is something to return.
        let (start, gap) = if cursor < self.dropped {
            // The bytes between `cursor` and `dropped` are gone; hand back the
            // whole live window and flag the discontinuity.
            (0usize, true)
        } else {
            ((cursor - self.dropped) as usize, false)
        };
        let bytes: Vec<u8> = self.buf.iter().skip(start).copied().collect();
        ReadSince {
            bytes,
            next: total,
            gap,
        }
    }
}

/// Result of [`BoundedRing::read_since`]: the new bytes, the cursor to use next,
/// and whether older bytes were dropped before this read (a stream gap).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadSince {
    pub bytes: Vec<u8>,
    pub next: u64,
    pub gap: bool,
}

/// Why the drainer thread stopped reading the PTY. The daemon (Wave 3) reads
/// this via [`PtySession::drain_outcome`] to distinguish a clean child exit
/// from a PTY fault and surface a `pty_drainer_errored` event. Without it, a
/// quietly-faulted PTY is indistinguishable from a healthy quiet agent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum DrainOutcome {
    /// Drainer still running.
    #[default]
    Running,
    /// Child closed the slave cleanly (read returned 0 bytes / EOF).
    Eof,
    /// A read error terminated the drainer. `kind`/`message` are preserved so
    /// the daemon can attribute the fault rather than guessing.
    Errored { kind: String, message: String },
}

/// A live PTY-managed child plus its output drainer. The owner (a worker
/// process under Outcome B) holds this for the child's lifetime.
pub struct PtySession {
    // Keep the master alive for the session: holding it keeps the fd open and
    // backs `resize`. Boxed trait object as returned by portable-pty.
    master: Box<dyn MasterPty + Send>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    child_pid: Option<u32>,
    ring: Arc<Mutex<BoundedRing>>,
    drain_outcome: Arc<Mutex<DrainOutcome>>,
    drainer: Option<JoinHandle<()>>,
}

impl PtySession {
    /// Spawn `cmd` on a fresh PTY of `rows`x`cols` with an output ring of
    /// `ring_bytes`, and start the drainer thread.
    pub fn spawn(
        cmd: CommandBuilder,
        rows: u16,
        cols: u16,
        ring_bytes: usize,
    ) -> Result<PtySession, PtyError> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| PtyError::OpenPty(e.to_string()))?;

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| PtyError::Spawn(e.to_string()))?;
        let child_pid = child.process_id();

        // Standard pattern: drop the slave so only the child holds it. The
        // master (held below) is the supervised-output side.
        drop(pair.slave);

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| PtyError::Writer(e.to_string()))?;
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| PtyError::Reader(e.to_string()))?;

        let ring = Arc::new(Mutex::new(BoundedRing::new(ring_bytes)));
        let drain_outcome = Arc::new(Mutex::new(DrainOutcome::Running));
        let drainer = spawn_drainer(reader, Arc::clone(&ring), Arc::clone(&drain_outcome))?;

        Ok(PtySession {
            master: pair.master,
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            child_pid,
            ring,
            drain_outcome,
            drainer: Some(drainer),
        })
    }

    /// Why the drainer stopped (or [`DrainOutcome::Running`] if still draining).
    /// The daemon turns an `Errored` outcome into a `pty_drainer_errored` event.
    pub fn drain_outcome(&self) -> DrainOutcome {
        match self.drain_outcome.lock() {
            Ok(o) => o.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// PID of the spawned child, if the platform reported one.
    pub fn child_pid(&self) -> Option<u32> {
        self.child_pid
    }

    /// Write bytes to the child's stdin (PTY master). The caller is responsible
    /// for any envelope wrapping (non-Claude providers).
    pub fn write_input(&self, bytes: &[u8]) -> Result<(), PtyError> {
        // Mutex poisoning would mean a prior writer panicked mid-write; treat
        // as a write failure rather than propagating a panic.
        let mut w = self
            .writer
            .lock()
            .map_err(|_| PtyError::Write(std::io::Error::other("writer mutex poisoned")))?;
        w.write_all(bytes).map_err(PtyError::Write)?;
        w.flush().map_err(PtyError::Write)?;
        Ok(())
    }

    /// Resize the PTY (drive resize handshake, terminal change).
    pub fn resize(&self, rows: u16, cols: u16) -> Result<(), PtyError> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| PtyError::Resize(e.to_string()))
    }

    /// Snapshot the current ring contents (oldest-to-newest).
    pub fn snapshot(&self) -> Vec<u8> {
        match self.ring.lock() {
            Ok(r) => r.snapshot(),
            Err(poisoned) => {
                // A prior holder panicked mid-mutation. Recover (availability
                // over propagation) but do not let the panic vanish silently.
                tracing::warn!("pty ring mutex poisoned; recovering for snapshot");
                poisoned.into_inner().snapshot()
            }
        }
    }

    /// Bytes dropped from the ring due to overflow.
    pub fn dropped_bytes(&self) -> u64 {
        match self.ring.lock() {
            Ok(r) => r.dropped_bytes(),
            Err(poisoned) => {
                tracing::warn!("pty ring mutex poisoned; recovering for dropped_bytes");
                poisoned.into_inner().dropped_bytes()
            }
        }
    }

    /// Incrementally read PTY output appended after `cursor` (drive streaming).
    /// See [`BoundedRing::read_since`]. A poisoned ring recovers in place rather
    /// than propagating a panic onto the drive output pump.
    pub fn read_since(&self, cursor: u64) -> ReadSince {
        match self.ring.lock() {
            Ok(r) => r.read_since(cursor),
            Err(poisoned) => {
                tracing::warn!("pty ring mutex poisoned; recovering for read_since");
                poisoned.into_inner().read_since(cursor)
            }
        }
    }

    /// True if the child has not yet exited.
    ///
    /// On a poisoned child mutex this returns `true` (assume alive), NOT
    /// `false`: the primary consumer is `Drop`, which kills the child only when
    /// this reports alive. Reporting `false` on poison would skip the kill and
    /// leak a still-running child. Erring toward "alive" makes `Drop` attempt
    /// the (idempotent) kill instead.
    pub fn is_child_alive(&self) -> bool {
        let mut child = match self.child.lock() {
            Ok(c) => c,
            Err(_) => {
                tracing::warn!("pty child mutex poisoned; assuming alive so Drop still kills");
                return true;
            }
        };
        // Return false ONLY when certain the child has exited (`Ok(Some(_))`).
        // A `try_wait` error (e.g. ECHILD if reaped elsewhere) errs toward
        // "alive" so `Drop`'s idempotent kill still fires rather than leaking a
        // lingering process.
        !matches!(child.try_wait(), Ok(Some(_)))
    }

    /// Block until the child exits, returning its exit code (if known).
    pub fn wait(&self) -> Result<u32, PtyError> {
        let mut child = self
            .child
            .lock()
            .map_err(|_| PtyError::Wait("child mutex poisoned".into()))?;
        let status = child.wait().map_err(|e| PtyError::Wait(e.to_string()))?;
        Ok(status.exit_code())
    }

    /// Kill the child (SIGKILL-equivalent via portable-pty).
    pub fn kill(&self) -> Result<(), PtyError> {
        let mut child = self
            .child
            .lock()
            .map_err(|_| PtyError::Kill("child mutex poisoned".into()))?;
        child.kill().map_err(|e| PtyError::Kill(e.to_string()))
    }

    /// Join the drainer thread with a bounded wait. If it does not terminate
    /// within `timeout` (e.g. the child is wedged in uninterruptible sleep so
    /// the blocking read never sees EOF), DETACH it rather than blocking the
    /// caller forever: drop the handle, log a warning. A leaked-but-logged
    /// thread beats a `Drop` that hangs the whole worker.
    fn join_drainer(&mut self, timeout: Duration) {
        if let Some(handle) = self.drainer.take() {
            let deadline = Instant::now() + timeout;
            while !handle.is_finished() {
                if Instant::now() >= deadline {
                    tracing::warn!(
                        "pty drainer did not terminate within {:?}; detaching (child may be in uninterruptible sleep)",
                        timeout
                    );
                    return; // detach: handle dropped without join
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = handle.join();
        }
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // Best-effort: if the child is still running, kill it so the drainer's
        // blocking read sees EOF and the thread can exit. Then join with a
        // bounded wait, detaching the drainer if the child is wedged in
        // uninterruptible sleep so `Drop` cannot hang the worker indefinitely.
        if self.is_child_alive() {
            let _ = self.kill();
        }
        self.join_drainer(DRAINER_JOIN_TIMEOUT);
    }
}

/// Spawn the drainer thread: read the PTY master to EOF, copying into the ring.
/// Records why it stopped into `drain_outcome` so a read fault is distinguishable
/// from a clean child exit (instead of both collapsing to a silent `break`).
fn spawn_drainer(
    mut reader: Box<dyn Read + Send>,
    ring: Arc<Mutex<BoundedRing>>,
    drain_outcome: Arc<Mutex<DrainOutcome>>,
) -> Result<JoinHandle<()>, PtyError> {
    std::thread::Builder::new()
        .name("fno-agents-pty-drainer".into())
        .spawn(move || {
            let mut buf = [0u8; 8192];
            let outcome = loop {
                match reader.read(&mut buf) {
                    Ok(0) => break DrainOutcome::Eof, // child closed the slave
                    Ok(n) => {
                        // Lock briefly to append; never hold across a read.
                        match ring.lock() {
                            Ok(mut r) => r.extend(&buf[..n]),
                            Err(poisoned) => {
                                tracing::warn!("pty ring mutex poisoned in drainer; recovering");
                                poisoned.into_inner().extend(&buf[..n]);
                            }
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        // On Linux a PTY master read returns EIO when the slave
                        // closes, which is the NORMAL child-exit termination
                        // (macOS returns Ok(0)). Treat EIO as a clean EOF so a
                        // healthy Linux exit is not misclassified as a PTY fault
                        // (which would wrongly drive pty_drainer_errored in the
                        // Wave 3 daemon). Any OTHER error is a real fault whose
                        // cause we preserve rather than discard.
                        if e.raw_os_error() == Some(libc::EIO) {
                            tracing::debug!("pty drainer read returned EIO (slave closed); treating as clean EOF");
                            break DrainOutcome::Eof;
                        }
                        tracing::warn!(error = %e, kind = ?e.kind(), "pty drainer read faulted");
                        break DrainOutcome::Errored {
                            kind: format!("{:?}", e.kind()),
                            message: e.to_string(),
                        };
                    }
                }
            };
            match drain_outcome.lock() {
                Ok(mut o) => *o = outcome,
                Err(poisoned) => *poisoned.into_inner() = outcome,
            }
        })
        // Thread creation only fails under OS resource exhaustion (EAGAIN/
        // ENOMEM). Return a typed error rather than panicking: this is library
        // code on a long-lived supervision path, so an avoidable panic would be
        // an availability failure for the worker/daemon.
        .map_err(|e| PtyError::Spawn(format!("drainer thread spawn failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_keeps_newest_on_overflow_and_counts_drops() {
        let mut ring = BoundedRing::new(4);
        ring.extend(b"abc");
        assert_eq!(ring.snapshot(), b"abc");
        assert_eq!(ring.dropped_bytes(), 0);
        ring.extend(b"de"); // "abcde" -> drop "a", keep "bcde"
        assert_eq!(ring.snapshot(), b"bcde");
        assert_eq!(ring.dropped_bytes(), 1);
    }

    #[test]
    fn ring_chunk_larger_than_capacity_keeps_tail() {
        let mut ring = BoundedRing::new(3);
        ring.extend(b"xy");
        ring.extend(b"123456"); // bigger than capacity; keep last 3 = "456"
        assert_eq!(ring.snapshot(), b"456");
        // dropped = the 2 buffered ("xy") + 3 leading of the chunk ("123")
        assert_eq!(ring.dropped_bytes(), 5);
    }

    #[test]
    fn ring_zero_capacity_clamps_to_one() {
        let mut ring = BoundedRing::new(0);
        assert_eq!(ring.capacity(), 1);
        ring.extend(b"ab");
        assert_eq!(ring.snapshot(), b"b");
    }

    #[test]
    fn read_since_streams_contiguous_appends() {
        let mut ring = BoundedRing::new(64);
        // Fresh reader starts at cursor 0.
        let r0 = ring.read_since(0);
        assert_eq!(r0.bytes, b"");
        assert_eq!(r0.next, 0);
        assert!(!r0.gap);

        ring.extend(b"hello");
        let r1 = ring.read_since(r0.next);
        assert_eq!(r1.bytes, b"hello");
        assert_eq!(r1.next, 5);
        assert!(!r1.gap);

        ring.extend(b" world");
        let r2 = ring.read_since(r1.next);
        assert_eq!(r2.bytes, b" world");
        assert_eq!(r2.next, 11);
        assert!(!r2.gap);

        // Re-reading at the tail yields nothing and does not rewind.
        let r3 = ring.read_since(r2.next);
        assert_eq!(r3.bytes, b"");
        assert_eq!(r3.next, 11);
        assert!(!r3.gap);
    }

    #[test]
    fn read_since_flags_gap_when_cursor_bytes_were_dropped() {
        let mut ring = BoundedRing::new(4);
        ring.extend(b"abcd"); // total=4, dropped=0
        let r1 = ring.read_since(0);
        assert_eq!(r1.bytes, b"abcd");
        assert_eq!(r1.next, 4);
        assert!(!r1.gap);

        // Overflow: "ef" pushes out "ab". total=6, dropped=2, buf="cdef".
        ring.extend(b"ef");
        // A reader stuck at cursor 0 lost bytes 0..2 ("ab"): gap, gets the live
        // window, advances to the tail.
        let stale = ring.read_since(0);
        assert!(stale.gap);
        assert_eq!(stale.bytes, b"cdef");
        assert_eq!(stale.next, 6);

        // A reader caught up at cursor 4 only missed nothing it had seen; it
        // gets "ef" with no gap (4 >= dropped=2).
        let fresh = ring.read_since(4);
        assert!(!fresh.gap);
        assert_eq!(fresh.bytes, b"ef");
        assert_eq!(fresh.next, 6);
    }

    #[test]
    fn read_since_future_cursor_is_noop() {
        let mut ring = BoundedRing::new(16);
        ring.extend(b"abc"); // total=3
        let r = ring.read_since(99);
        assert_eq!(r.bytes, b"");
        assert_eq!(r.next, 99);
        assert!(!r.gap);
    }
}
