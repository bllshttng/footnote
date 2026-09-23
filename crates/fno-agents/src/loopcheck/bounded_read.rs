//! The bounded transport for synchronous external reads on the stop path:
//! spawn, poll `try_wait`, kill the child process group on expiry, and
//! classify the outcome as a type. Moved verbatim out of `loopcheck.rs`
//! (shrink-only under the file budget); policy stays at the call site.

use crate::bounded_spawn::{kill_process_group, killpg, spawn_bounded};
use std::ffi::OsStr;
use std::io::Read as _;
use std::path::Path;

/// Cap on RETAINED stderr per bounded run. Only retention is capped - the
/// drain itself always runs to EOF, or a child that overflows the pipe would
/// deadlock before exiting. The tail (not the head) is kept because the end
/// of a diagnostic stream carries the line that killed the run.
pub(crate) const BOUNDED_STDERR_TAIL_CAP: usize = 2000;

/// A completed bounded run's full classification payload: exit status, stdout,
/// and a capped stderr tail. Parsers read `stdout`; the status and tail exist
/// so a non-zero exit or a failing child can be NAMED at the call site rather
/// than collapsed into "the read failed".
pub(crate) struct BoundedOutput {
    pub(crate) status: std::process::ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr_tail: Vec<u8>,
}

/// Outcome of a bounded, killable child run: the whole point is that a hang
/// inside the child can never
/// again read as "the read failed" or wedge forever - it reads as exactly
/// what happened, with the verb and the elapsed time attached at the call
/// site.
pub(crate) enum BoundedRun {
    Completed(BoundedOutput),
    TimedOut(std::time::Duration),
    /// The io error kind is kept because "binary absent" (NotFound) and
    /// "could not spawn right now" (ETXTBSY, EACCES, ...) are different
    /// facts; collapsing them is how a transient spawn failure used to read
    /// as absence at the gh probe.
    SpawnFailed(std::io::ErrorKind),
    /// `try_wait()` itself errored (e.g. a concurrent reap of the group
    /// leader) - the bound was never reached, so this must stay distinct
    /// from `TimedOut` or a wait failure would misreport as "timed out
    /// after 0s", naming a hang that never happened.
    WaitFailed,
}

/// Run `fno_bin args...` under a native wall-clock bound, killing the
/// child's whole process group on expiry - the same discipline as
/// `run_probe` (spawn, poll `try_wait`, `kill_process_group`) - and return
/// the completed payload (exit status, stdout, capped stderr tail). This is
/// the single transport boundary for synchronous external work on the stop
/// path: `Command::output()` alone has no timeout, so it blocks until the
/// child exits however long that takes, which is exactly how a hang three
/// calls deep in `fno do plan fidelity` turned into loop-check
/// itself hanging forever and stranding the session behind it. stdout and
/// stderr are drained on background threads for the same reason `run_probe`
/// drains stderr that way: reading a pipe only after the child exits
/// deadlocks against a child that fills the pipe buffer before exiting.
pub(crate) fn run_bounded(
    fno_bin: &OsStr,
    args: &[&str],
    cwd: &Path,
    timeout: std::time::Duration,
) -> BoundedRun {
    let mut child = match crate::bounded_spawn::spawn_bounded(fno_bin, args, cwd) {
        Ok(c) => c,
        Err(kind) => return BoundedRun::SpawnFailed(kind),
    };
    let pgid = child.id() as i32;

    let mut stdout_pipe = child.stdout.take();
    let stdout_drain = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(ref mut p) = stdout_pipe {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });
    let mut stderr_pipe = child.stderr.take();
    let stderr_drain = std::thread::spawn(move || {
        // Retain only the LAST `BOUNDED_STDERR_TAIL_CAP` bytes while still
        // draining to EOF: an unbounded write must not buy an unbounded
        // allocation, but stopping the read early would block a child that
        // is still writing, deadlocking the runner before any timeout.
        let mut tail: Vec<u8> = Vec::new();
        if let Some(ref mut p) = stderr_pipe {
            let mut chunk = [0u8; 4096];
            loop {
                match p.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        // Always append, THEN trim: a guard on append would
                        // freeze the buffer at whatever the first chunks
                        // carried, which for a stream longer than one chunk
                        // keeps the head, not the tail.
                        tail.extend_from_slice(&chunk[..n]);
                        if tail.len() > BOUNDED_STDERR_TAIL_CAP {
                            let excess = tail.len() - BOUNDED_STDERR_TAIL_CAP;
                            tail.drain(..excess);
                        }
                    }
                }
            }
        }
        tail
    });

    enum Outcome {
        Done(std::process::ExitStatus),
        TimedOut(std::time::Duration),
        WaitFailed,
    }

    let start = std::time::Instant::now();
    let outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Outcome::Done(status),
            Ok(None) => {
                let elapsed = start.elapsed();
                if elapsed >= timeout {
                    kill_process_group(&mut child);
                    break Outcome::TimedOut(elapsed);
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => {
                // The bound was never reached - this is NOT a timeout, and
                // must not be reported as one (that would name a hang that
                // never happened). Still kill the group: an error mid-wait
                // leaves the child's liveness unknown, and a stray survivor
                // must not outlive this call.
                kill_process_group(&mut child);
                break Outcome::WaitFailed;
            }
        }
    };

    // Reap any descendant still holding a pipe write end (a wrapper script
    // that forks) so the drain threads see EOF either way.
    killpg(pgid);

    match outcome {
        // Captured at the moment the bound was actually crossed, not after
        // kill_process_group + killpg have run - else the reported duration
        // is inflated by cleanup cost instead of reflecting the timeout itself.
        Outcome::TimedOut(elapsed) => BoundedRun::TimedOut(elapsed),
        Outcome::WaitFailed => BoundedRun::WaitFailed,
        Outcome::Done(status) => {
            let stdout = stdout_drain.join().unwrap_or_default();
            let stderr_tail = stderr_drain.join().unwrap_or_default();
            BoundedRun::Completed(BoundedOutput {
                status,
                stdout,
                stderr_tail,
            })
        }
    }
}

/// How an external stop-gate read failed. `TimedOut` is its own kind so a
/// killed child can never render through the ordinary failed-read wording -
/// the two demand opposite operator responses (wait out a reset vs. debug a
/// command), and conflating them is how a hang reads as a blip forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadErrorKind {
    /// Non-zero exit or unparseable payload. The ordinary vocabulary
    /// ("gh read '<name>' failed; retrying next fire").
    Failed,
    /// The process could not be spawned or waited on.
    Unrunnable,
    /// The child outlived its bound and the process group was killed.
    TimedOut,
}

/// One external read's typed failure: the logical read name, the kind, the
/// capped stderr tail, and - for a timeout - the elapsed bound. Threads
/// through every stop-gate reader so the render sites classify instead of
/// guessing from a detail string.
#[derive(Clone)]
pub(crate) struct GhReadError {
    pub(crate) read: String,
    pub(crate) kind: ReadErrorKind,
    pub(crate) stderr_tail: String,
    pub(crate) elapsed: Option<std::time::Duration>,
    /// The raw io error kind when a spawn failed, so a caller can tell
    /// "binary absent" (NotFound) from "could not spawn right now". None for
    /// every other failure class, including wait failures.
    pub(crate) spawn_kind: Option<std::io::ErrorKind>,
}

impl GhReadError {
    pub(crate) fn failed(read: &str, stderr_tail: String) -> Self {
        GhReadError {
            read: read.to_string(),
            kind: ReadErrorKind::Failed,
            stderr_tail,
            elapsed: None,
            spawn_kind: None,
        }
    }

    pub(crate) fn parse_failed(read: &str) -> Self {
        Self::failed(read, String::new())
    }

    pub(crate) fn timed_out(read: &str, elapsed: std::time::Duration) -> Self {
        GhReadError {
            read: read.to_string(),
            kind: ReadErrorKind::TimedOut,
            stderr_tail: String::new(),
            elapsed: Some(elapsed),
            spawn_kind: None,
        }
    }

    pub(crate) fn unrunnable(read: &str, detail: &str) -> Self {
        GhReadError {
            read: read.to_string(),
            kind: ReadErrorKind::Unrunnable,
            stderr_tail: detail.to_string(),
            elapsed: None,
            spawn_kind: None,
        }
    }

    pub(crate) fn unrunnable_spawn(
        read: &str,
        spawn_kind: std::io::ErrorKind,
        detail: &str,
    ) -> Self {
        GhReadError {
            read: read.to_string(),
            kind: ReadErrorKind::Unrunnable,
            stderr_tail: detail.to_string(),
            elapsed: None,
            spawn_kind: Some(spawn_kind),
        }
    }

    /// The kill bound when this error is a timeout, so a render site can
    /// classify without parsing the rendered prose.
    pub(crate) fn timeout_bound(&self) -> Option<std::time::Duration> {
        match self.kind {
            ReadErrorKind::TimedOut => self.elapsed,
            _ => None,
        }
    }

    pub(crate) fn render(&self) -> String {
        match self.kind {
            ReadErrorKind::TimedOut => format!(
                "external read '{}' timed out after {:.1}s and was killed; retrying next fire",
                self.read,
                self.elapsed.map(|d| d.as_secs_f64()).unwrap_or(0.0)
            ),
            ReadErrorKind::Failed => format!(
                "gh read '{}' failed; retrying next fire. {}",
                self.read, self.stderr_tail
            ),
            ReadErrorKind::Unrunnable => format!(
                "external read '{}' could not run; retrying next fire. {}",
                self.read, self.stderr_tail
            ),
        }
    }

    /// The positive outcome marker for `loop_check_gh_error` rows: every row
    /// carries one, so a timeout is distinguishable in the events log without
    /// parsing prose.
    pub(crate) fn outcome(&self) -> &'static str {
        match self.kind {
            ReadErrorKind::TimedOut => "timeout",
            ReadErrorKind::Failed => "failed",
            ReadErrorKind::Unrunnable => "unrunnable",
        }
    }
}

pub(crate) fn bounded_read_diagnostic(context: &str, error: &GhReadError) -> String {
    let elapsed = error
        .elapsed
        .map(|duration| format!("{:.1}", duration.as_secs_f64()))
        .unwrap_or_else(|| "-".to_string());
    format!(
        "loop-check: {context}: external read '{}' outcome={} elapsed_s={} stderr_tail={:?}",
        error.read,
        error.outcome(),
        elapsed,
        error.stderr_tail
    )
}

pub(crate) fn log_bounded_read_error(context: &str, error: &GhReadError) {
    eprintln!("{}", bounded_read_diagnostic(context, error));
}

/// One external read through the single bounded transport. Every synchronous
/// `gh`/`fno` read reachable from the stop decision routes through here, so
/// none can hang the fire and none can misreport a kill as an ordinary
/// failure. The completed payload carries the exit status, stdout, and the
/// pre-capped stderr tail; policy (fail-closed, fail-open, degrade) stays at
/// the typed call site.
pub(crate) fn bounded_read(
    bin: &OsStr,
    args: &[&str],
    cwd: &Path,
    read_name: &str,
    timeout: std::time::Duration,
) -> Result<BoundedOutput, GhReadError> {
    match run_bounded(bin, args, cwd, timeout) {
        BoundedRun::Completed(out) => Ok(out),
        BoundedRun::TimedOut(elapsed) => Err(GhReadError::timed_out(read_name, elapsed)),
        BoundedRun::SpawnFailed(kind) => Err(GhReadError::unrunnable_spawn(
            read_name,
            kind,
            &format!("spawn failed ({kind:?})"),
        )),
        BoundedRun::WaitFailed => Err(GhReadError::unrunnable(read_name, "wait failed")),
    }
}
