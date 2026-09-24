//! How does a child process run under a bound? Bounded subprocess runs, bounded gh reads and their errors, and the gh binary probe.

use super::*;

/// Cap on RETAINED stderr per bounded run. Only retention is capped - the
/// drain itself always runs to EOF, or a child that overflows the pipe would
/// deadlock before exiting. The tail (not the head) is kept because the end
/// of a diagnostic stream carries the line that killed the run.
pub(super) const BOUNDED_STDERR_TAIL_CAP: usize = 2000;

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
pub(super) enum BoundedRun {
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
pub(super) fn run_bounded(
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
pub(super) enum ReadErrorKind {
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
    pub(super) read: String,
    pub(super) kind: ReadErrorKind,
    pub(super) stderr_tail: String,
    pub(super) elapsed: Option<std::time::Duration>,
    /// The raw io error kind when a spawn failed, so a caller can tell
    /// "binary absent" (NotFound) from "could not spawn right now". None for
    /// every other failure class, including wait failures.
    pub(super) spawn_kind: Option<std::io::ErrorKind>,
}

impl GhReadError {
    pub(super) fn failed(read: &str, stderr_tail: String) -> Self {
        GhReadError {
            read: read.to_string(),
            kind: ReadErrorKind::Failed,
            stderr_tail,
            elapsed: None,
            spawn_kind: None,
        }
    }

    pub(super) fn parse_failed(read: &str) -> Self {
        Self::failed(read, String::new())
    }

    pub(super) fn timed_out(read: &str, elapsed: std::time::Duration) -> Self {
        GhReadError {
            read: read.to_string(),
            kind: ReadErrorKind::TimedOut,
            stderr_tail: String::new(),
            elapsed: Some(elapsed),
            spawn_kind: None,
        }
    }

    pub(super) fn unrunnable(read: &str, detail: &str) -> Self {
        GhReadError {
            read: read.to_string(),
            kind: ReadErrorKind::Unrunnable,
            stderr_tail: detail.to_string(),
            elapsed: None,
            spawn_kind: None,
        }
    }

    pub(super) fn unrunnable_spawn(
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
    pub(super) fn outcome(&self) -> &'static str {
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

/// The stop gate's answer to "is gh installed?" Three states, because a
/// transient spawn failure and an absent binary are different claims (the
/// failure-encoded-as-value class): Absent means the OS answered NotFound;
/// SpawnTrouble means gh exists but could not be spawned right now (ETXTBSY
/// while the binary is still being written, EACCES, ...); Present means the
/// probe spawned at all - completion at any exit code OR a timeout both
/// prove the binary exists, since the child had to run to hit either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum GhProbeOutcome {
    Present,
    Absent,
    SpawnTrouble { kind: std::io::ErrorKind },
}

impl GhProbeOutcome {
    /// The positive outcome marker for the `gh_probe` event row: each state
    /// has its own string, so the probe's conclusion is readable from the
    /// events log without parsing prose.
    pub(super) fn outcome_str(&self) -> &'static str {
        match self {
            GhProbeOutcome::Present => "found",
            GhProbeOutcome::Absent => "absent",
            GhProbeOutcome::SpawnTrouble { .. } => "spawn_trouble",
        }
    }

    pub(super) fn detail_str(&self) -> String {
        match self {
            GhProbeOutcome::SpawnTrouble { kind } => format!("{kind:?}"),
            _ => String::new(),
        }
    }
}

/// Probe gh by spawning `gh --version` through the bounded transport. Only
/// NotFound reads as Absent, and absence is stable so it is not retried.
/// Every other spawn error is retried a bounded number of times (ETXTBSY
/// clears in milliseconds) and then reported as SpawnTrouble - never as
/// absence, because "could not spawn right now" is not a fact about the
/// world. Callers treat SpawnTrouble as present: the downstream reads are
/// individually bounded and each carries its own conservative failure
/// handling, which is exactly where a still-broken spawn belongs.
pub(super) fn probe_gh_bin(gh_bin: &OsStr, cwd: &Path) -> GhProbeOutcome {
    let mut last_kind = std::io::ErrorKind::Other;
    for _ in 0..3 {
        match bounded_read(
            gh_bin,
            &["--version"],
            cwd,
            "gh_version_probe",
            std::time::Duration::from_secs(5),
        ) {
            // Completion proves existence at any exit code; a timeout proves
            // it too (the child ran and outlived its bound), and a Failed
            // read can only follow a completed spawn.
            Ok(_)
            | Err(GhReadError {
                kind: ReadErrorKind::Failed | ReadErrorKind::TimedOut,
                ..
            }) => return GhProbeOutcome::Present,
            Err(GhReadError {
                kind: ReadErrorKind::Unrunnable,
                spawn_kind: Some(std::io::ErrorKind::NotFound),
                ..
            }) => {
                // ENOENT is ambiguous: the binary is absent, or it EXISTS
                // and its shebang interpreter is. An existing file is spawn
                // trouble, never absence - a gh present on disk must not
                // degrade the session to advisory mode.
                if path_lookup(gh_bin).is_some() {
                    return GhProbeOutcome::SpawnTrouble {
                        kind: std::io::ErrorKind::NotFound,
                    };
                }
                return GhProbeOutcome::Absent;
            }
            Err(GhReadError {
                kind: ReadErrorKind::Unrunnable,
                spawn_kind,
                ..
            }) => {
                last_kind = spawn_kind.unwrap_or(std::io::ErrorKind::Other);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }
    GhProbeOutcome::SpawnTrouble { kind: last_kind }
}

/// Resolve `bin` the way `Command::new` would: a path with a separator is
/// checked directly, a bare name is searched on PATH (first regular-file
/// hit). Used only on the NotFound arm of the gh probe, to tell "no such
/// file" from "the file exists but execve said ENOENT" (missing interpreter).
pub(super) fn path_lookup(bin: &OsStr) -> Option<std::path::PathBuf> {
    let name = bin.to_str()?;
    if name.contains('/') {
        return std::fs::symlink_metadata(name)
            .ok()
            .map(|_| std::path::PathBuf::from(name));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

/// One bounded local-`git` read. Every stop-gate git call routes here for
/// the same reason the gh reads route through `bounded_read`: an external
/// diff driver, a locked index, or a stalled mount can hang `git` exactly
/// the way a wedged network child hangs `gh`, and an unbounded `.output()`
/// turns that into a fire that never decides. A timeout or unrunnable git is
/// named on stderr (the shim's forensic log) and reads as "no answer";
/// every caller already treats no-answer as its conservative outcome
/// (unshipped, dirty, unknown branch), so the degrade direction is the same
/// one each caller documented for a failing git.
pub(crate) fn git_bounded(git_bin: &str, args: &[&str], cwd: &Path) -> Option<BoundedOutput> {
    let read_name = format!("git {}", args.first().unwrap_or(&"?"));
    match run_bounded(OsStr::new(git_bin), args, cwd, stopgate_read_timeout()) {
        BoundedRun::Completed(out) => Some(out),
        BoundedRun::TimedOut(elapsed) => {
            let error = GhReadError::timed_out(&read_name, elapsed);
            log_bounded_read_error("git", &error);
            None
        }
        BoundedRun::SpawnFailed(kind) => {
            let error = GhReadError::unrunnable_spawn(
                &read_name,
                kind,
                &format!("spawn failed ({kind:?})"),
            );
            log_bounded_read_error("git", &error);
            None
        }
        BoundedRun::WaitFailed => {
            let error = GhReadError::unrunnable(&read_name, "wait failed");
            log_bounded_read_error("git", &error);
            None
        }
    }
}
