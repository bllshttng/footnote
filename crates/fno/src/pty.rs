//! PTY spawn + reader thread for the mux server.
//!
//! Seeded from `crates/fno-agents/src/pty.rs`, adapted for the mux: there is
//! no output ring here - the VT grid (`vt.rs`) is the retention - so the
//! reader thread forwards raw chunks straight into a bounded tokio channel
//! the server core loop consumes. PTY master reads are blocking, so they live
//! on a dedicated thread and tokio stays at the edges (the herdr model). When
//! the channel is full the reader blocks, which backpressures the child via
//! the kernel PTY buffer - bounded memory, never an unbounded server-side
//! queue (AC2-EDGE).

use crate::mux_cli::{BASH_SHELL_INIT, ZSH_SHELL_INIT};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Serializes every PTY fork in this process.
///
/// `spawn_command` forks, then runs a `pre_exec` hook in the child before
/// `exec`. Only async-signal-safe calls are legal in that window, but
/// portable-pty's hook calls `close_random_fds`, which reads `/dev/fd` and so
/// allocates. Forking a MULTITHREADED process while another thread holds the
/// allocator lock is undefined behaviour, and it surfaces as the child acting on
/// an inconsistent fd list: `EBADF` from a pty that opened fine, panes that fail
/// to spawn for no reason. Both our processes are multithreaded - the server
/// runs tokio, the test binary runs a thread per test - so the forks must not
/// overlap. Held across the fork only (~1ms), never across the child's life, so
/// concurrent panes still run concurrently once spawned.
///
/// Poison is recovered rather than propagated: one panicking spawn must not turn
/// every later spawn into a second failure.
static FORK_LOCK: Mutex<()> = Mutex::new(());

thread_local! {
    // Instant of the last openpty timeout, scoped per thread rather than a
    // process-wide static. In production this is a no-op difference: the
    // mux server's single-threaded core loop is the only caller, so a
    // thread-local and a process-wide value hold identical state. In this
    // crate's test binary it matters a great deal - `cargo test` runs many
    // threads concurrently, and a process-wide cooldown let one test's
    // deliberately-injected wedge get read as a real wedge by an unrelated,
    // healthy `server::tests` spawn racing it on another thread (observed:
    // CI failures like "openpty skipped: it timed out 2ms ago" in tests that
    // never touch the wedge machinery). Thread-local closes that class of
    // failure by construction, not by narrowing a window.
    //
    // While fresher than one timeout window, further opens on the same
    // thread refuse immediately instead of paying the deadline again: a
    // wedged host wedges every pty, not just the first. Self-clearing, so a
    // host that recovers needs no reset.
    static LAST_OPENPTY_TIMEOUT: std::cell::Cell<Option<std::time::Instant>> =
        const { std::cell::Cell::new(None) };
}

/// Record that an openpty call just timed out, starting the cooldown window.
fn note_wedge() {
    LAST_OPENPTY_TIMEOUT.with(|c| c.set(Some(std::time::Instant::now())));
}

/// `Some(elapsed)` when the last timeout is still fresher than `timeout`,
/// `None` when there was no recent timeout or the cooldown has expired.
fn wedge_cooldown(timeout: std::time::Duration) -> Option<std::time::Duration> {
    let elapsed = LAST_OPENPTY_TIMEOUT.with(|c| c.get())?.elapsed();
    (elapsed < timeout).then_some(elapsed)
}

thread_local! {
    // Count of openpty calls that have timed out, ever (this thread only -
    // see LAST_OPENPTY_TIMEOUT above). The cooldown only throttles retry
    // *rate*; a host that stays wedged past the cooldown window would
    // otherwise leak one more thread every window, forever, which is the
    // failure mode this module exists to prevent.
    //
    // Deliberately monotonic and single-thread-written, unlike an earlier
    // version of this fix that tracked a *live* in-flight count incremented
    // by the caller and decremented by the offloaded probe thread on
    // completion: that required cross-thread bookkeeping, and this crate's
    // own test for it proved the hazard - the test's 64 probe threads,
    // still sleeping a 60s injected hang, decremented the shared counter
    // long after the test itself had returned and reset it for the next
    // test, underflowing an AtomicUsize and silently wedging every later
    // open_pty call in the same process. A count that only ever goes up,
    // written only from the timeout branch below (always this thread),
    // cannot underflow and needs no cross-thread handoff.
    static TOTAL_TIMEOUTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Once a thread has seen this many of its own openpty calls time out,
/// further calls refuse immediately without spawning another probe: a host
/// this persistently wedged is not going to recover on the next attempt, so
/// there is nothing more to learn by leaking another thread. Comfortably
/// above anything a real incident's retry cadence would hit by accident.
const MAX_TIMEOUTS_BEFORE_GIVING_UP: usize = 200;

#[cfg(test)]
struct TestHome(PathBuf);

#[cfg(test)]
impl TestHome {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "fno-pty-test-home-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create isolated PTY test HOME");
        Self(dir)
    }
}

#[cfg(test)]
impl Drop for TestHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
thread_local! {
    static TEST_HOME: TestHome = TestHome::new();
}

fn fork_guard() -> std::sync::MutexGuard<'static, ()> {
    FORK_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    #[error("failed to open pty: {0}")]
    OpenPty(String),
    #[error(
        "failed to open pty: this mux server is at its open-file ceiling \
         ({open} of {limit} fds in use). Raise it with setrlimit(RLIMIT_NOFILE) at \
         server start, or restart the server to reclaim descriptors (this kills live panes). \
         `fno agents spawn --substrate bg` needs no pane. Underlying: {detail}"
    )]
    OpenPtyFdLimit {
        open: usize,
        limit: libc::rlim_t,
        detail: String,
    },
    #[error(
        "failed to spawn pty child: this mux server is at its open-file ceiling \
         ({open} of {limit} fds in use). Raise it with setrlimit(RLIMIT_NOFILE) at \
         server start, or restart the server to reclaim descriptors (this kills live panes). \
         `fno agents spawn --substrate bg` needs no pane. Underlying: {detail}"
    )]
    SpawnFdLimit {
        open: usize,
        limit: libc::rlim_t,
        detail: String,
    },
    #[error(
        "openpty did not return within {0:?}; the host pty layer may be wedged (check: ls /dev). \
             This pane was refused, the mux server is still serving"
    )]
    OpenPtyTimeout(std::time::Duration),
    #[error(
        "openpty skipped: it timed out {0:?} ago and the host pty layer is still suspect \
             (check: ls /dev). Retry once ls /dev returns"
    )]
    OpenPtyWedged(std::time::Duration),
    #[error(
        "openpty skipped: {0} of this thread's openpty calls have already timed out \
             (check: ls /dev). This pty layer is not recovering; no further probes will be spawned"
    )]
    OpenPtyGivenUp(usize),
    #[error("no spawnable shell: {0}")]
    Spawn(String),
    #[error("failed to obtain pty writer: {0}")]
    Writer(String),
    #[error("failed to obtain pty reader: {0}")]
    Reader(String),
    #[error("pty write failed: {0}")]
    Write(std::io::Error),
    #[error("pty resize failed: {0}")]
    Resize(String),
}

/// Resolve the shell candidates to try, in order: `$SHELL` (when set and
/// non-empty) then `/bin/sh` (AC1-ERR fallback). `OsStr` in/out so a non-UTF-8
/// shell path survives without lossy conversion (gemini). Pure so it is
/// unit-testable.
pub fn shell_candidates(env_shell: Option<&OsStr>) -> Vec<OsString> {
    let mut v = Vec::new();
    if let Some(s) = env_shell {
        let is_empty = match s.to_str() {
            Some(utf8) => utf8.trim().is_empty(),
            None => s.is_empty(),
        };
        if !is_empty {
            v.push(s.to_os_string());
        }
    }
    if !v.iter().any(|s| s == OsStr::new("/bin/sh")) {
        v.push(OsString::from("/bin/sh"));
    }
    v
}

/// A live PTY-managed shell. The server owns this for the pane's lifetime -
/// client attach/detach never touches it (AC4-HP).
pub struct PtyShell {
    // Held for the fd + resize; boxed trait object as returned by portable-pty.
    master: Box<dyn MasterPty + Send>,
    // Input goes through a dedicated writer thread (spawn_writer): a PTY
    // master write blocks when the kernel input buffer is full, and doing
    // that on the tokio core loop could deadlock the whole server against a
    // child that is itself blocked on output (gemini high). Bounded queue:
    // when a pathological child stops reading input, keystrokes drop
    // (fail-closed, same policy as input-after-exit) - never unbounded memory.
    input_tx: std::sync::mpsc::SyncSender<Vec<u8>>,
    child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    reader_done: Arc<AtomicBool>,
    // Shell-integration rc temp dir, held purely for cleanup (RAII): dropped
    // when the pane closes / the server exits. `None` for non-shell / off panes.
    _shell_rc: Option<ShellRc>,
}

impl PtyShell {
    /// Spawn the first spawnable candidate from `candidates` on a fresh
    /// `rows`x`cols` PTY, starting the shell in `cwd` when given and still a
    /// directory (a vanished dir degrades to the server's cwd rather than
    /// refusing the pane). Output chunks flow into the SHARED `out_tx` tagged
    /// with `pane_id`; when the child is gone (EOF/EIO on the master) the
    /// reader thread sends `pane_id` on `exit_tx` and ends. An explicit exit
    /// channel replaces Phase 1's channel-close signal: with one shared
    /// output channel for N panes, one reader exiting can no longer close it.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        candidates: &[OsString],
        rows: u16,
        cols: u16,
        cwd: Option<&std::path::Path>,
        session: &str,
        pane_id: u64,
        out_tx: tokio::sync::mpsc::Sender<(u64, Vec<u8>)>,
        exit_tx: tokio::sync::mpsc::Sender<u64>,
    ) -> Result<PtyShell, PtyError> {
        let permit =
            crate::process_admission::admit_fleet().map_err(|e| PtyError::Spawn(e.to_string()))?;
        Self::spawn_with_permit(
            candidates, rows, cols, cwd, session, pane_id, out_tx, exit_tx, permit,
        )
    }

    /// Spawn after the server has performed the combined fleet/tab decision.
    /// The permit remains alive through the PTY child-creation syscall.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_with_permit(
        candidates: &[OsString],
        rows: u16,
        cols: u16,
        cwd: Option<&std::path::Path>,
        session: &str,
        pane_id: u64,
        out_tx: tokio::sync::mpsc::Sender<(u64, Vec<u8>)>,
        exit_tx: tokio::sync::mpsc::Sender<u64>,
        permit: crate::process_admission::AdmissionPermit,
    ) -> Result<PtyShell, PtyError> {
        let pair = open_pty(rows, cols)?;
        let mut errors = Vec::new();
        let mut child = None;
        let mut shell_rc = None;
        let fork = fork_guard();
        for cand in candidates {
            let mut cmd = base_command(cand, cwd, session, pane_id);
            let rc = apply_shell_integration(&mut cmd, cand, session, pane_id);
            match pair.slave.spawn_command(cmd) {
                Ok(c) => {
                    child = Some(c);
                    shell_rc = rc;
                    break;
                }
                // rc drops here on failure -> its temp dir is removed.
                Err(e) if is_typed_emfile(e.as_ref()) => {
                    drop(fork);
                    return Err(classify_spawn_fd_ceiling(format!(
                        "{}: {e}",
                        cand.to_string_lossy()
                    )));
                }
                Err(e) => errors.push(format!("{}: {e}", cand.to_string_lossy())),
            }
        }
        drop(fork);
        let mut child = child.ok_or_else(|| PtyError::Spawn(errors.join("; ")))?;
        if let Some(pid) = child.process_id() {
            if let Err(error) = permit.record_child(pid) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(PtyError::Spawn(format!("admission marker failed: {error}")));
            }
        }
        wire(pair, child, pane_id, out_tx, exit_tx, shell_rc)
    }

    /// Spawn an explicit command (`argv[0]` + args) as a pane - the
    /// `fno mux pane run` / agents-spawn primitive. No shell fallback: an
    /// unspawnable argv is the caller's error, surfaced verbatim. Env + cwd
    /// (incl. `FNO_SESSION`) match [`PtyShell::spawn`].
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_cmd(
        argv: &[String],
        rows: u16,
        cols: u16,
        cwd: Option<&std::path::Path>,
        session: &str,
        pane_id: u64,
        out_tx: tokio::sync::mpsc::Sender<(u64, Vec<u8>)>,
        exit_tx: tokio::sync::mpsc::Sender<u64>,
    ) -> Result<PtyShell, PtyError> {
        let permit =
            crate::process_admission::admit_fleet().map_err(|e| PtyError::Spawn(e.to_string()))?;
        Self::spawn_cmd_with_permit(
            argv, rows, cols, cwd, session, pane_id, out_tx, exit_tx, permit,
        )
    }

    /// Spawn an explicit command after a server-owned admission decision.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_cmd_with_permit(
        argv: &[String],
        rows: u16,
        cols: u16,
        cwd: Option<&std::path::Path>,
        session: &str,
        pane_id: u64,
        out_tx: tokio::sync::mpsc::Sender<(u64, Vec<u8>)>,
        exit_tx: tokio::sync::mpsc::Sender<u64>,
        permit: crate::process_admission::AdmissionPermit,
    ) -> Result<PtyShell, PtyError> {
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| PtyError::Spawn("empty argv".into()))?;
        let pair = open_pty(rows, cols)?;
        let mut cmd = base_command(OsStr::new(program), cwd, session, pane_id);
        // A bare shell argv (`pane run -- zsh`) is wrapped; a non-shell argv (an
        // agent TUI) passes through untouched. Wrap before the caller's args so
        // bash's `--rcfile` leads.
        let shell_rc = apply_shell_integration(&mut cmd, OsStr::new(program), session, pane_id);
        for a in args {
            cmd.arg(a);
        }
        let mut child = {
            let _fork = fork_guard();
            pair.slave.spawn_command(cmd).map_err(|e| {
                let detail = format!("{program}: {e}");
                if is_typed_emfile(e.as_ref()) {
                    classify_spawn_fd_ceiling(detail)
                } else {
                    PtyError::Spawn(detail)
                }
            })?
        };
        if !crate::process_admission::is_root_program(OsStr::new(program)) {
            if let Some(pid) = child.process_id() {
                if let Err(error) = permit.record_child(pid) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(PtyError::Spawn(format!("admission marker failed: {error}")));
                }
            }
        }
        wire(pair, child, pane_id, out_tx, exit_tx, shell_rc)
    }

    /// The child's OS process id, when the platform reported one. Read for
    /// `pane ls`; `None` never means "dead" (use [`PtyShell::is_child_alive`]).
    pub fn child_pid(&self) -> Option<u32> {
        self.child.lock().ok().and_then(|c| c.process_id())
    }

    /// Queue keystrokes for the child (the writer thread performs the actual
    /// blocking write, so this never blocks the caller). The caller decides
    /// what a failure means (the server drops input fail-closed once the
    /// child has exited, AC2-ERR); a full queue drops the chunk the same way.
    pub fn write_input(&self, bytes: &[u8]) -> Result<(), PtyError> {
        use std::sync::mpsc::TrySendError;
        self.input_tx.try_send(bytes.to_vec()).map_err(|e| match e {
            TrySendError::Full(_) => PtyError::Write(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "pty input queue full; chunk dropped",
            )),
            TrySendError::Disconnected(_) => PtyError::Write(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "pty writer thread gone (child exited)",
            )),
        })
    }

    /// Resize the PTY. Pixel dimensions ride along for programs that read
    /// `ws_xpixel`/`ws_ypixel`; pass 0 when unknown (the Phase-1 protocol does
    /// not carry pixels yet).
    pub fn resize(
        &self,
        rows: u16,
        cols: u16,
        pixel_width: u16,
        pixel_height: u16,
    ) -> Result<(), PtyError> {
        self.master
            .resize(PtySize {
                rows: rows.max(1),
                cols: cols.max(1),
                pixel_width,
                pixel_height,
            })
            .map_err(|e| PtyError::Resize(e.to_string()))
    }

    /// True while the child has not exited. A poisoned lock still owns a valid
    /// child handle, so recover it. An OS inspection error remains fail-safe
    /// ("alive") but is reported instead of silently defeating the reaper.
    pub fn is_child_alive(&self) -> bool {
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        match child.try_wait() {
            Ok(Some(_)) => false,
            Ok(None) => true,
            Err(e) => {
                eprintln!(
                    "fno mux: cannot inspect pane child {:?}; retaining pane: {e}",
                    child.process_id()
                );
                true
            }
        }
    }

    /// True only after the child is dead and the PTY reader has enqueued every
    /// final output chunk. The acquire pairs with the reader's release store,
    /// letting the server snapshot safe reaper candidates before draining its
    /// output channel and closing panes.
    pub fn is_reap_ready(&self) -> bool {
        self.reader_done.load(Ordering::Acquire) && !self.is_child_alive()
    }

    /// Kill and reap the child (explicit ClosePane / CloseTab). Idempotent:
    /// killing an already-dead child errors harmlessly and the wait reaps
    /// either way, so a close racing a natural exit never double-reaps or
    /// leaves a zombie. SIGKILL makes the post-kill wait effectively
    /// immediate, so this is safe on the core loop.
    pub fn kill(&self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// How long `open_pty` waits for the kernel before refusing the pane.
/// A healthy openpty returns in well under a millisecond, so this is four
/// orders of magnitude of headroom, chosen short enough that a burst of
/// spawns against a wedged host costs seconds rather than minutes.
const OPENPTY_TIMEOUT_MS: u64 = 5_000;

/// `FNO_MUX_OPENPTY_TIMEOUT_MS` overrides `OPENPTY_TIMEOUT_MS`. A zero or
/// unparseable value reads as the default rather than as no timeout, since
/// "no timeout" is the bug this exists to fix.
fn openpty_timeout() -> std::time::Duration {
    let ms = std::env::var("FNO_MUX_OPENPTY_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&ms| ms > 0)
        .unwrap_or(OPENPTY_TIMEOUT_MS);
    std::time::Duration::from_millis(ms)
}

thread_local! {
    // Fault-injection knob for tests: seeded once per thread from
    // `FNO_MUX_OPENPTY_HANG_MS`, so the shipped `--server` binary can drive
    // it too (this is a runtime read, not `#[cfg(test)]`) and one test's
    // injection never poisons a sibling test on another thread.
    static OPENPTY_HANG: std::cell::Cell<std::time::Duration> = std::cell::Cell::new(
        std::env::var("FNO_MUX_OPENPTY_HANG_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(std::time::Duration::from_millis)
            .unwrap_or_default(),
    );
}

fn hang_injection() -> std::time::Duration {
    OPENPTY_HANG.with(|c| c.get())
}

#[cfg(test)]
fn set_hang_injection(d: std::time::Duration) {
    OPENPTY_HANG.with(|c| c.set(d));
}

#[cfg(test)]
fn clear_wedge_cooldown() {
    LAST_OPENPTY_TIMEOUT.with(|c| c.set(None));
}

#[cfg(test)]
fn set_total_timeouts(n: usize) {
    TOTAL_TIMEOUTS.with(|c| c.set(n));
}

/// The soft `RLIMIT_NOFILE` to install, or `None` when the current soft limit
/// already stands at the installable ceiling. `cap` is the platform's
/// per-process file cap, when it has one separate from the hard limit.
fn target_fd_limit(
    soft: libc::rlim_t,
    hard: libc::rlim_t,
    cap: Option<libc::rlim_t>,
) -> Option<libc::rlim_t> {
    let target = cap.map_or(hard, |cap| hard.min(cap));
    if target == libc::RLIM_INFINITY {
        return None;
    }
    (target > soft).then_some(target)
}

/// macOS reports an infinite hard limit even though the kernel enforces a
/// finite per-process maximum. Read that installable ceiling directly.
#[cfg(target_vendor = "apple")]
fn fd_cap() -> Option<libc::rlim_t> {
    let mut mib = [libc::CTL_KERN, libc::KERN_MAXFILESPERPROC];
    let mut cap: libc::c_int = 0;
    let mut cap_len = std::mem::size_of_val(&cap);
    // SAFETY: `mib` names one integer sysctl and the output buffer is a live
    // `c_int` whose exact size is supplied through `cap_len`.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            &mut cap as *mut _ as *mut libc::c_void,
            &mut cap_len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && cap > 0).then_some(cap as libc::rlim_t)
}

#[cfg(not(target_vendor = "apple"))]
fn fd_cap() -> Option<libc::rlim_t> {
    None
}

fn fd_limits() -> Result<(libc::rlim_t, libc::rlim_t), std::io::Error> {
    let mut limits = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    // SAFETY: `getrlimit` initializes the supplied `rlimit` on success.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limits.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the successful call above initialized the value.
    let limits = unsafe { limits.assume_init() };
    Ok((limits.rlim_cur, limits.rlim_max))
}

/// Count descriptors without opening another descriptor. The diagnostic runs
/// only after EMFILE, where `/dev/fd` cannot be opened and an accept loop can
/// consume any descriptor released from a traditional emergency reserve.
fn count_open_fds(limit: libc::rlim_t) -> usize {
    let upper = limit.min(libc::c_int::MAX as libc::rlim_t) as libc::c_int;
    (0..upper)
        .filter(|fd| {
            // SAFETY: F_GETFD only inspects the integer descriptor and does not
            // read through a pointer or allocate a new descriptor.
            let rc = unsafe { libc::fcntl(*fd, libc::F_GETFD) };
            rc != -1 || std::io::Error::last_os_error().raw_os_error() != Some(libc::EBADF)
        })
        .count()
}

/// Raise this process's soft open-file limit to the installable ceiling.
/// Returns the values read before and after the change, never the requested
/// value in place of the kernel's answer.
pub fn raise_fd_limit() -> Result<Option<(libc::rlim_t, libc::rlim_t)>, std::io::Error> {
    let (before, hard) = fd_limits()?;
    let Some(target) = target_fd_limit(before, hard, fd_cap()) else {
        return Ok(None);
    };
    let limits = libc::rlimit {
        rlim_cur: target,
        rlim_max: hard,
    };
    // SAFETY: `limits` preserves the current hard limit and only raises the
    // soft limit to a value bounded by both the hard and platform limits.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limits) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let (after, _) = fd_limits()?;
    Ok((after > before).then_some((before, after)))
}

/// `portable-pty` formats the original `io::Error` into text, so EMFILE is no
/// longer available for downcasting here. Match both forms its debug string
/// can retain.
fn is_fd_ceiling(message: &str) -> bool {
    message.contains("Too many open files") || message.contains("code: 24,")
}

fn is_typed_emfile(mut error: &(dyn std::error::Error + 'static)) -> bool {
    loop {
        if error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.raw_os_error() == Some(libc::EMFILE))
        {
            return true;
        }
        let Some(source) = error.source() else {
            return false;
        };
        error = source;
    }
}

fn fd_ceiling_state() -> Option<(usize, libc::rlim_t)> {
    let (limit, _) = fd_limits().ok()?;
    let open = count_open_fds(limit);
    Some((open, limit))
}

fn classify_open_pty_error(detail: String) -> PtyError {
    if is_fd_ceiling(&detail) {
        if let Some((open, limit)) = fd_ceiling_state() {
            return PtyError::OpenPtyFdLimit {
                open,
                limit,
                detail,
            };
        }
    }
    PtyError::OpenPty(detail)
}

fn classify_spawn_fd_ceiling(detail: String) -> PtyError {
    if let Some((open, limit)) = fd_ceiling_state() {
        PtyError::SpawnFdLimit {
            open,
            limit,
            detail,
        }
    } else {
        PtyError::Spawn(detail)
    }
}

/// Open a fresh PTY pair at `rows`x`cols` (clamped to >=1).
///
/// The real `openpty(3)` call runs on a throwaway thread with a bounded
/// wait, because it is a blocking syscall against a kernel resource
/// (`/dev/tty*`) that can wedge, and this runs on the server's single
/// core-loop thread: without the offload, a wedge stalls that thread, and
/// so the server, forever (see the module-level incident writeup this
/// fixes). This bounds the stall to `timeout` instead of removing it - the
/// core loop is synchronous, so every other client is still frozen for that
/// window - but the server recovers and refuses that one pane afterward,
/// rather than staying wedged indefinitely.
fn open_pty(rows: u16, cols: u16) -> Result<portable_pty::PtyPair, PtyError> {
    let timeout = openpty_timeout();
    if let Some(ago) = wedge_cooldown(timeout) {
        return Err(PtyError::OpenPtyWedged(ago));
    }
    let total_timeouts = TOTAL_TIMEOUTS.with(|c| c.get());
    if total_timeouts >= MAX_TIMEOUTS_BEFORE_GIVING_UP {
        // This many of this thread's own openpty calls have already timed
        // out; spawning yet another leaked thread teaches us nothing a real
        // host recovery wouldn't already show up as a fresh cooldown expiry.
        return Err(PtyError::OpenPtyGivenUp(total_timeouts));
    }
    let size = PtySize {
        rows: rows.max(1),
        cols: cols.max(1),
        pixel_width: 0,
        pixel_height: 0,
    };
    let hang = hang_injection();
    // Buffered (capacity 1), never a rendezvous channel: a late thread must
    // be able to hand off and exit even after the receiver gave up, so the
    // dropped PtyPair closes its fds instead of the sender parking forever.
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new()
        .name("fno-mux-openpty".into())
        .spawn(move || {
            if !hang.is_zero() {
                std::thread::sleep(hang);
            }
            let result = native_pty_system().openpty(size).map_err(|e| e.to_string());
            // Send may fail when we already timed out; the pair drops here
            // and its fds close, so a late success leaks nothing.
            let _ = tx.send(result);
        });
    if let Err(e) = spawned {
        return Err(PtyError::OpenPty(format!(
            "could not start the openpty thread: {e}"
        )));
    }
    match rx.recv_timeout(timeout) {
        Ok(Ok(pair)) => {
            // A live open proves the host isn't wedged right now, so past
            // timeouts stop counting toward the give-up threshold. Without
            // this, MAX_TIMEOUTS_BEFORE_GIVING_UP accumulates across the
            // thread's entire lifetime rather than one sustained incident -
            // a server that recovers from several unrelated wedge episodes
            // over weeks of uptime would otherwise permanently refuse every
            // later pane, healthy host or not, until restarted.
            TOTAL_TIMEOUTS.with(|c| c.set(0));
            Ok(pair)
        }
        Ok(Err(e)) => Err(classify_open_pty_error(e)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            // This thread's spawned probe leaks (an uninterruptible kernel
            // sleep no timeout can cancel); MAX_TIMEOUTS_BEFORE_GIVING_UP
            // bounds how many times that can happen before this thread
            // stops trying, instead of leaking one more per cooldown window
            // forever.
            TOTAL_TIMEOUTS.with(|c| c.set(c.get() + 1));
            note_wedge();
            Err(PtyError::OpenPtyTimeout(timeout))
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(PtyError::OpenPty(
            "the openpty thread ended without answering (it panicked before replying)".into(),
        )),
    }
}

/// A `CommandBuilder` for `program` carrying the pane environment: a login-ish
/// `TERM`, `FNO_SESSION` (AC3-HP: the nested-attach guard and kill-server both
/// read it), `FNO_PANE` (4a-G2: the pane's own id, so an agent hosted in a
/// pane can name itself to the registry and an in-pane spawn can address its
/// host), `FNO_PANE_EPOCH` (a per-spawn nanosecond token so the turn-marker
/// hook can tell THE pane host from a nested `claude -p` that
/// inherited FNO_PANE - pane ids recycle across server restarts, the epoch
/// does not), and the launch cwd when it is still a directory.
fn base_command(
    program: &OsStr,
    cwd: Option<&std::path::Path>,
    session: &str,
    pane_id: u64,
) -> CommandBuilder {
    let mut cmd = CommandBuilder::new(program);
    #[cfg(test)]
    TEST_HOME.with(|home| {
        cmd.env("HOME", &home.0);
        cmd.env("USERPROFILE", &home.0);
    });
    cmd.env("TERM", "xterm-256color");
    // Stated, never inherited. `CommandBuilder::new` seeds from the SERVER's
    // `std::env::vars_os()`, so without this a pane's color depends on where
    // the detached `fno --server` happened to be launched from - and a server
    // reparented to init carries whatever env that launch had, forever. The
    // symptom is a hosted TUI rendering monochrome while `TERM` still claims
    // 256 colors, because `COLORTERM` is the signal apps actually read
    // (`mux_cli::truecolor_check` says so). We already assert `TERM` here; a
    // pane that advertises 256 colors and withholds the 24-bit signal is
    // asserting half a capability. Stating both is one unconditional rule,
    // which cannot be wrong about a launch context the way inheritance can.
    cmd.env("COLORTERM", "truecolor");
    // And CLEAR the suppression, which is the half that actually greys panes.
    // `NO_COLOR` is honored by nearly every TUI, and it OUTRANKS the two lines
    // above: stating TERM and COLORTERM while `NO_COLOR=1` rides along still
    // renders monochrome. It arrives by inheritance the same way, and the
    // usual source is an agent shell - this harness exports `NO_COLOR=1`, so a
    // server an agent restarts is detached at PPID 1 carrying it for life, and
    // hands it to every pane it will ever spawn. Measured 2026-08-21: a pane
    // env held `NO_COLOR=1` with `COLORTERM=` empty while the outer terminal
    // had neither problem.
    //
    // A pane host IS a terminal. It has no business inheriting a caller's
    // decision to suppress color in that caller's own output.
    cmd.env_remove("NO_COLOR");
    cmd.env("FNO_SESSION", session);
    cmd.env("FNO_PANE", pane_id.to_string());
    // Unique per pane-host spawn: descendants (incl. a nested claude) inherit
    // the same value, so the hook's first-writer pin is keyed to THIS host and
    // a recycled pane_id after a server restart gets a fresh key.
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    cmd.env("FNO_PANE_EPOCH", epoch.to_string());
    if let Some(dir) = cwd.filter(|d| d.is_dir()) {
        cmd.cwd(dir);
    }
    cmd
}

/// A shell the mux knows how to inject OSC 133 block markers into.
enum ShellKind {
    Zsh,
    Bash,
}

/// Map the basename of `program` to a known shell. `None` for an agent TUI or
/// any non-shell argv, which passes through un-wrapped.
fn shell_kind(program: &OsStr) -> Option<ShellKind> {
    match std::path::Path::new(program).file_name()?.to_str()? {
        "zsh" => Some(ShellKind::Zsh),
        "bash" => Some(ShellKind::Bash),
        _ => None,
    }
}

/// The knob's only off-switch: `FNO_MUX_SHELL_INTEGRATION=off`. Anything else
/// (absent, `mux-panes`, garbage) reads as on - the feature's default.
fn integration_disabled(knob: Option<&OsStr>) -> bool {
    knob == Some(OsStr::new("off"))
}

/// The temp `.zshenv`: source the user's real `.zshenv` by explicit path, and
/// deliberately do NOT restore `ZDOTDIR` (that stays pointed at the temp dir so
/// zsh reads OUR `.zshrc` next, not the user's).
const ZSH_ZSHENV: &str =
    "[ -f \"${USER_ZDOTDIR:-$HOME}/.zshenv\" ] && . \"${USER_ZDOTDIR:-$HOME}/.zshenv\"\n";

/// The temp `.zshrc`: restore `ZDOTDIR` when the user exported one, else UNSET
/// it - a non-exporting user's rc must see the same pristine state a plain
/// terminal gives it, so `${ZDOTDIR:-default}` modular-config idioms fire.
/// Force-setting `ZDOTDIR=$HOME` here killed those configs silently. Source the
/// user rc from `${USER_ZDOTDIR:-$HOME}` (NOT `$ZDOTDIR`, which may now be
/// unset), then eval the snippet LAST so it wins the prompt.
fn zsh_zshrc() -> String {
    format!(
        "if [ -n \"${{USER_ZDOTDIR:-}}\" ]; then ZDOTDIR=\"$USER_ZDOTDIR\"; else unset ZDOTDIR; fi\n\
         [ -f \"${{USER_ZDOTDIR:-$HOME}}/.zshrc\" ] && . \"${{USER_ZDOTDIR:-$HOME}}/.zshrc\"\n\
         {ZSH_SHELL_INIT}"
    )
}

/// The temp bash `--rcfile`: source the user's `~/.bashrc` then the snippet.
fn bash_rcfile_body() -> String {
    format!("[ -f \"$HOME/.bashrc\" ] && . \"$HOME/.bashrc\"\n{BASH_SHELL_INIT}")
}

/// A per-pane temp dir holding the shell-integration rc file(s). Held by the
/// owning `PtyShell` only for cleanup: the rc is read once at shell startup, so
/// it just has to outlive the child's first few ms, which the pane always does.
/// Dropped -> removed when the pane closes / the server exits.
struct ShellRc {
    dir: PathBuf,
}

impl Drop for ShellRc {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Inject the OSC 133 snippet into a mux-spawned shell, and ONLY that shell -
/// never the user's global rc. zsh: a temp `ZDOTDIR` whose `.zshenv` /
/// `.zshrc` source the user's real files (`USER_ZDOTDIR`, or `$HOME` when
/// unset) then eval the snippet LAST, so a prompt owner (starship/p10k) runs
/// first and our `B` marker is appended after. bash: a temp `--rcfile` that
/// sources `~/.bashrc` then the snippet. The snippet's `_FNO_OSC133` guard makes
/// a shell that already emits OSC 133 a no-op, and the scanner is
/// emitter-agnostic. Off when `FNO_MUX_SHELL_INTEGRATION=off`; absent reads as
/// on (`mux-panes`). Fail-open: a temp-write error skips integration rather than
/// failing the pane spawn.
///
/// ponytail: a user whose own `.zshenv` sets `ZDOTDIR` defeats the temp-dir hop
/// (zsh then reads their `.zshrc`, not ours) - the same known limit as VS Code's
/// scheme; the manual `fno mux shell-init` eval is the upgrade path there.
fn apply_shell_integration(
    cmd: &mut CommandBuilder,
    program: &OsStr,
    session: &str,
    pane_id: u64,
) -> Option<ShellRc> {
    if integration_disabled(std::env::var_os("FNO_MUX_SHELL_INTEGRATION").as_deref()) {
        return None;
    }
    let kind = shell_kind(program)?;
    // Under the per-user 0700 mux dir, NOT world-writable /tmp: a shell that
    // sources these rc files is an RCE surface, so a predictable path in a
    // shared temp dir (where an attacker could pre-create the dir and swap the
    // rc) is CWE-377. `ensure_private_dir` forces 0700 on both levels, and no
    // other uid can enter the parent, so the per-pane name being predictable is
    // safe. Unique per pane (session + id); a crashed server's leftover of the
    // same name is removed first, never appended to.
    let dir = crate::proto::mux_dir()
        .join("shell-rc")
        .join(format!("fno-mux-{session}-{pane_id}"));
    let _ = fs::remove_dir_all(&dir);
    crate::proto::ensure_private_dir(&dir).ok()?;
    let rc = ShellRc { dir };
    match kind {
        ShellKind::Zsh => {
            fs::write(rc.dir.join(".zshenv"), ZSH_ZSHENV).ok()?;
            fs::write(rc.dir.join(".zshrc"), zsh_zshrc()).ok()?;
            cmd.env("ZDOTDIR", &rc.dir);
            // Preserve the user's real ZDOTDIR for the temp rc to source; unset
            // -> the in-shell `${USER_ZDOTDIR:-$HOME}` falls back to $HOME.
            if let Some(z) = std::env::var_os("ZDOTDIR") {
                cmd.env("USER_ZDOTDIR", z);
            }
        }
        ShellKind::Bash => {
            let rcfile = rc.dir.join("bashrc");
            fs::write(&rcfile, bash_rcfile_body()).ok()?;
            cmd.arg("--rcfile");
            cmd.arg(rcfile);
        }
    }
    Some(rc)
}

/// Wire a spawned child's PTY into the mux: drop the slave (only the child
/// holds it), start the reader + writer threads, and hand back the owning
/// [`PtyShell`]. Shared by [`PtyShell::spawn`] and [`PtyShell::spawn_cmd`].
fn wire(
    pair: portable_pty::PtyPair,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    pane_id: u64,
    out_tx: tokio::sync::mpsc::Sender<(u64, Vec<u8>)>,
    exit_tx: tokio::sync::mpsc::Sender<u64>,
    shell_rc: Option<ShellRc>,
) -> Result<PtyShell, PtyError> {
    // Standard pattern: drop the slave so only the child holds it.
    drop(pair.slave);
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| PtyError::Writer(e.to_string()))?;
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| PtyError::Reader(e.to_string()))?;
    let reader_done = Arc::new(AtomicBool::new(false));
    spawn_reader(reader, pane_id, out_tx, exit_tx, Arc::clone(&reader_done))?;
    let input_tx = spawn_writer(writer)?;
    Ok(PtyShell {
        master: pair.master,
        input_tx,
        child: Mutex::new(child),
        reader_done,
        _shell_rc: shell_rc,
    })
}

/// The blocking writer thread: bounded channel -> PTY master. Owns the writer
/// so its blocking `write_all`/`flush` never runs on the tokio core loop
/// (gemini high: a full kernel input buffer + a child blocked on output is a
/// deadlock if the core loop does the write). Exits when the channel closes
/// (PtyShell dropped) or a write fails (child gone).
fn spawn_writer(
    mut writer: Box<dyn Write + Send>,
) -> Result<std::sync::mpsc::SyncSender<Vec<u8>>, PtyError> {
    // 256 keystroke-sized chunks of headroom; beyond that the child has
    // stopped reading input for a long while and dropping is the safe answer.
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(256);
    std::thread::Builder::new()
        .name("fno-mux-pty-writer".into())
        .spawn(move || {
            while let Ok(bytes) = rx.recv() {
                if writer
                    .write_all(&bytes)
                    .and_then(|()| writer.flush())
                    .is_err()
                {
                    break; // child/master gone; senders see Disconnected
                }
            }
        })
        .map_err(|e| PtyError::Writer(format!("writer thread spawn failed: {e}")))?;
    Ok(tx)
}

/// The blocking reader thread: PTY master -> the shared pane-tagged channel.
/// Exits on EOF (child gone; macOS reports Ok(0), Linux EIO) or a closed
/// channel, sending the pane id on `exit_tx` either way so the core loop
/// learns WHICH pane's child ended.
fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    pane_id: u64,
    out_tx: tokio::sync::mpsc::Sender<(u64, Vec<u8>)>,
    exit_tx: tokio::sync::mpsc::Sender<u64>,
    reader_done: Arc<AtomicBool>,
) -> Result<(), PtyError> {
    std::thread::Builder::new()
        .name("fno-mux-pty-reader".into())
        .spawn(move || {
            // x-0296 diagnostics: the first chunk logged from THIS std thread
            // proves the child spoke even when the tokio runtime is wedged.
            let e2e = std::env::var_os("FNO_E2E").is_some();
            let drop_exit = e2e && std::env::var_os("FNO_E2E_DROP_PTY_EXIT").is_some();
            let mut first = true;
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if e2e && std::mem::take(&mut first) {
                            eprintln!("fno mux e2e: pty reader pane {pane_id}: first {n} bytes");
                        }
                        if e2e {
                            let delay_ms = std::env::var("FNO_E2E_PTY_OUTPUT_DELAY_MS")
                                .ok()
                                .and_then(|value| value.parse::<u64>().ok())
                                .unwrap_or(0);
                            if delay_ms > 0 {
                                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                            }
                        }
                        // blocking_send backpressures the reader (and thus the
                        // child) when the core loop lags; never unbounded.
                        if out_tx.blocking_send((pane_id, buf[..n].to_vec())).is_err() {
                            break; // consumer gone; nothing to drain for
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        // Linux: EIO when the slave side closes = normal exit.
                        if e.raw_os_error() != Some(libc::EIO) {
                            eprintln!("fno mux: pty read faulted: {e}");
                        }
                        break;
                    }
                }
            }
            reader_done.store(true, Ordering::Release);
            if drop_exit {
                eprintln!("fno mux e2e: deliberately dropped exit for pane {pane_id}");
            } else {
                let _ = exit_tx.blocking_send(pane_id);
            }
        })
        .map_err(|e| PtyError::Reader(format!("reader thread spawn failed: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    /// Serializes every test in this module that reaches `open_pty`. The
    /// wedge cooldown itself is thread-local (see `LAST_OPENPTY_TIMEOUT`
    /// above), so it can't leak between tests on different threads; what
    /// this gate protects is `FNO_MUX_OPENPTY_TIMEOUT_MS`, a process-wide
    /// env var with no thread-local equivalent - two wedge tests setting it
    /// concurrently would race (see `PtyTestGuard` below). Poison recovered
    /// rather than propagated, matching `fork_guard`.
    // tokio::sync::Mutex, not std::sync::Mutex: the `#[tokio::test]` spawn
    // tests hold this guard across an `.await`, which clippy's
    // `await_holding_lock` correctly refuses for a std lock (it can deadlock
    // an executor). `blocking_lock` below is the sync `#[test]` fns' way in -
    // it panics only inside an async context, and these tests have none.
    static PTY_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// A pane must STATE its color capability, never inherit it.
    ///
    /// The regression this pins: `CommandBuilder::new` seeds from the server
    /// process's own `std::env::vars_os()`, so a detached `fno --server`
    /// reparented to init hands every pane whatever env its launch context
    /// had, for the life of that server. A server started without `COLORTERM`
    /// rendered every hosted TUI monochrome while `TERM` still advertised 256
    /// colors - half a capability, asserted.
    ///
    /// Asserts the VALUES, not merely that the keys exist. `TERM` set and
    /// `COLORTERM` missing IS the broken state, so a test checking only
    /// `is_some()` on each would have passed throughout the bug.
    ///
    /// The sentinel is load-bearing and this test is worthless without it.
    /// `CommandBuilder::new` seeds from `std::env::vars_os()`, so a plain
    /// `get_env("COLORTERM") == "truecolor"` assertion reads the value this
    /// TEST PROCESS inherited from the developer's terminal and passes
    /// whether or not the code under test sets anything. Verified: with
    /// `cmd.env("COLORTERM", ...)` deleted, that naive form still passed.
    /// Poisoning the parent env with a value the code must OVERRIDE is what
    /// separates "stated by base_command" from "inherited from whoever ran
    /// the test" - the exact distinction the shipped bug turned on.
    /// A pane must CLEAR `NO_COLOR`, not merely state the two positives.
    ///
    /// `NO_COLOR` outranks `TERM` and `COLORTERM`: a pane carrying all three
    /// renders monochrome anyway, which is why stating the positives alone
    /// left the reported bug alive. It arrives by inheritance, and the usual
    /// source is an agent shell that exports it - a server an agent restarts
    /// is detached at PPID 1 and hands `NO_COLOR=1` to every pane for life.
    ///
    /// The sentinel here is the parent's own `NO_COLOR=1`. Without setting it
    /// first, a machine whose developer has no `NO_COLOR` passes this test
    /// with `env_remove` deleted, because there was nothing to remove.
    #[test]
    fn pane_env_clears_inherited_no_color() {
        let _gate = PTY_GATE.blocking_lock();
        let restore = std::env::var_os("NO_COLOR");
        std::env::set_var("NO_COLOR", "1");

        let cmd = base_command(OsStr::new("/bin/sh"), None, "main", 7);
        let no_color = cmd.get_env("NO_COLOR").map(|v| v.to_owned());

        match restore {
            Some(v) => std::env::set_var("NO_COLOR", v),
            None => std::env::remove_var("NO_COLOR"),
        }

        assert_eq!(
            no_color, None,
            "a pane must CLEAR an inherited NO_COLOR; it outranks TERM and \
             COLORTERM, so a pane that carries it renders monochrome even \
             with both positives stated"
        );
    }

    #[test]
    fn pane_env_states_both_color_signals() {
        // Serialized: `set_var`/`remove_var` are process-wide, and this is the
        // same hazard PTY_GATE already exists to serialize.
        let _gate = PTY_GATE.blocking_lock();
        let restore = std::env::var_os("COLORTERM");
        std::env::set_var("COLORTERM", "fno-test-sentinel-must-be-overridden");

        let cmd = base_command(OsStr::new("/bin/sh"), None, "main", 7);
        let term = cmd.get_env("TERM").map(|v| v.to_owned());
        let colorterm = cmd.get_env("COLORTERM").map(|v| v.to_owned());

        // Restore before asserting so a failure cannot leak the sentinel into
        // every test that runs after this one.
        match restore {
            Some(v) => std::env::set_var("COLORTERM", v),
            None => std::env::remove_var("COLORTERM"),
        }

        assert_eq!(
            term.as_deref(),
            Some(OsStr::new("xterm-256color")),
            "a pane must state TERM"
        );
        assert_eq!(
            colorterm.as_deref(),
            Some(OsStr::new("truecolor")),
            "a pane must STATE COLORTERM and override the parent's value; \
             inheriting it leaves a hosted TUI monochrome whenever the \
             detached server was launched without one"
        );
    }

    /// Reset the globals a wedge test can leave dirty: the cooldown Instant,
    /// the hang-injection thread-local a worker thread may have carried over
    /// from an earlier test, and the monotonic give-up counter
    /// (`TOTAL_TIMEOUTS`). Zeroing the counter here can under-count a
    /// genuinely concurrent probe from another test running at this exact
    /// instant, which only weakens the cap momentarily - never a spurious
    /// refusal - so that's an acceptable trade for keeping this crate's
    /// wedge tests from permanently eating into the cap themselves.
    fn reset_pty_test_globals() {
        clear_wedge_cooldown();
        set_hang_injection(std::time::Duration::ZERO);
        TOTAL_TIMEOUTS.with(|c| c.set(0));
        // Also covers FNO_MUX_OPENPTY_TIMEOUT_MS: a test that sets it and
        // then panics before its own remove_var would otherwise leave every
        // later open_pty call in the process reading a 50-200ms deadline
        // instead of the 5s default, failing unrelated tests under load.
        // The Drop side of this (see PtyTestGuard) makes that panic-safe.
        std::env::remove_var("FNO_MUX_OPENPTY_TIMEOUT_MS");
    }

    /// A held `PTY_GATE` guard that also resets the wedge globals on drop,
    /// not just on acquire.
    ///
    /// `LAST_OPENPTY_TIMEOUT` and `TOTAL_TIMEOUTS` are thread-local (see
    /// their own doc comments), so a wedge test running on its own `cargo
    /// test` worker thread cannot leak either into a concurrently-running
    /// `server::tests` case on another thread - that class of cross-test
    /// pollution is closed by construction, not by this gate. What the gate
    /// still serializes is `FNO_MUX_OPENPTY_TIMEOUT_MS`: `std::env::set_var`
    /// has no thread-local equivalent, so two wedge tests setting it to
    /// different values concurrently would race, and the reset-on-drop here
    /// (not just on acquire) makes that panic-safe - a wedge test that
    /// panics mid-body still clears its override instead of leaving a
    /// 50-200ms deadline for whichever test runs on this same thread next.
    struct PtyTestGuard(#[allow(dead_code)] tokio::sync::MutexGuard<'static, ()>);

    impl Drop for PtyTestGuard {
        fn drop(&mut self) {
            reset_pty_test_globals();
        }
    }

    /// Gate for the plain `#[test]` fns in this module.
    fn pty_gate() -> PtyTestGuard {
        let guard = PTY_GATE.blocking_lock();
        reset_pty_test_globals();
        PtyTestGuard(guard)
    }

    /// Gate for the `#[tokio::test]` fns, which need to hold it across an
    /// `.await`.
    async fn pty_gate_async() -> PtyTestGuard {
        let guard = PTY_GATE.lock().await;
        reset_pty_test_globals();
        PtyTestGuard(guard)
    }

    #[test]
    fn server_spine_shell_candidates_prefer_env_then_sh() {
        assert_eq!(
            shell_candidates(Some(OsStr::new("/bin/zsh"))),
            vec![OsString::from("/bin/zsh"), OsString::from("/bin/sh")]
        );
        assert_eq!(shell_candidates(None), vec![OsString::from("/bin/sh")]);
        assert_eq!(
            shell_candidates(Some(OsStr::new(""))),
            vec![OsString::from("/bin/sh")]
        );
        assert_eq!(
            shell_candidates(Some(OsStr::new("  "))),
            vec![OsString::from("/bin/sh")]
        );
        // No duplicate when $SHELL already is /bin/sh.
        assert_eq!(
            shell_candidates(Some(OsStr::new("/bin/sh"))),
            vec![OsString::from("/bin/sh")]
        );
    }

    #[test]
    fn fd_limit_target_clamps_infinite_hard_limit_to_platform_cap() {
        assert_eq!(
            target_fd_limit(256, libc::RLIM_INFINITY, Some(245_760)),
            Some(245_760)
        );
    }

    #[test]
    fn fd_limit_target_refuses_uninstallable_infinity() {
        assert_eq!(target_fd_limit(256, libc::RLIM_INFINITY, None), None);
    }

    #[test]
    fn fd_limit_target_is_idempotent_at_ceiling() {
        assert_eq!(target_fd_limit(4_096, 4_096, Some(8_192)), None);
        assert_eq!(target_fd_limit(4_096, 8_192, Some(4_096)), None);
    }

    #[test]
    fn fd_count_scans_existing_descriptors_without_a_reserve() {
        let marker = std::fs::File::open("/dev/null").unwrap();
        let marker_fd = marker.as_raw_fd();
        let count = count_open_fds(marker_fd as libc::rlim_t + 1);
        assert!(count > 0);
        // The scan observes the marker without consuming or replacing it.
        assert_ne!(unsafe { libc::fcntl(marker_fd, libc::F_GETFD) }, -1);
    }

    #[test]
    fn openpty_emfile_text_is_classified_as_fd_ceiling() {
        assert!(is_fd_ceiling(
            "failed to openpty: Os { code: 24, kind: Uncategorized, message: Too many open files }"
        ));
        assert!(is_fd_ceiling(
            "failed to openpty: Os { code: 24, kind: Uncategorized }"
        ));
        assert!(!is_fd_ceiling(
            "failed to openpty: Os { code: 2, kind: NotFound, message: No such file or directory }"
        ));
    }

    #[test]
    fn fd_ceiling_error_names_usage_limit_and_remedy() {
        let message = PtyError::OpenPtyFdLimit {
            open: 255,
            limit: 256,
            detail: "Too many open files".into(),
        }
        .to_string();
        assert!(message.contains("255 of 256"), "{message}");
        assert!(message.contains("setrlimit"), "{message}");
        assert!(message.contains("open-file ceiling"), "{message}");
    }

    #[test]
    fn shell_integration_detects_only_known_shells() {
        // basename maps zsh/bash (bare or full-path); everything else is a
        // non-shell argv that passes through un-wrapped.
        assert!(matches!(
            shell_kind(OsStr::new("zsh")),
            Some(ShellKind::Zsh)
        ));
        assert!(matches!(
            shell_kind(OsStr::new("/bin/zsh")),
            Some(ShellKind::Zsh)
        ));
        assert!(matches!(
            shell_kind(OsStr::new("/usr/local/bin/bash")),
            Some(ShellKind::Bash)
        ));
        assert!(shell_kind(OsStr::new("claude")).is_none());
        assert!(shell_kind(OsStr::new("/usr/bin/nvim")).is_none());
        assert!(shell_kind(OsStr::new("fish")).is_none());
    }

    #[test]
    fn shell_integration_off_knob_only_matches_off() {
        // Only the literal "off" disables; absent / mux-panes / garbage read on.
        assert!(integration_disabled(Some(OsStr::new("off"))));
        assert!(!integration_disabled(None));
        assert!(!integration_disabled(Some(OsStr::new("mux-panes"))));
        assert!(!integration_disabled(Some(OsStr::new(""))));
        assert!(!integration_disabled(Some(OsStr::new("on"))));
    }

    #[test]
    fn shell_integration_rc_embeds_snippet_and_sources_user_rc() {
        // zsh: restore-or-unset ZDOTDIR, source the user's .zshrc from
        // ${USER_ZDOTDIR:-$HOME} (not $ZDOTDIR, which may be unset), snippet LAST.
        let zshrc = zsh_zshrc();
        assert!(zshrc.starts_with("if [ -n \"${USER_ZDOTDIR:-}\" ]; then ZDOTDIR=\"$USER_ZDOTDIR\"; else unset ZDOTDIR; fi"));
        assert!(zshrc.contains(". \"${USER_ZDOTDIR:-$HOME}/.zshrc\""));
        assert!(zshrc.contains("_FNO_OSC133"));
        assert!(zshrc.trim_end().ends_with(ZSH_SHELL_INIT.trim_end()));
        // .zshenv must NOT restore ZDOTDIR (keeps zsh reading our .zshrc next).
        assert!(!ZSH_ZSHENV.contains("ZDOTDIR="));
        assert!(ZSH_ZSHENV.contains(".zshenv"));
        // bash: source the user's ~/.bashrc then the snippet.
        let bashrc = bash_rcfile_body();
        assert!(bashrc.contains("$HOME/.bashrc"));
        assert!(bashrc.contains("_FNO_OSC133"));
    }

    /// Write a per-pane mux-hop ZDOTDIR (mirrors `apply_shell_integration`) and
    /// return its path. Caller removes it.
    #[cfg(test)]
    fn write_hop_zdotdir(tag: &str) -> PathBuf {
        let hop = std::env::temp_dir().join(format!("fno-test-hop-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&hop);
        fs::create_dir_all(&hop).unwrap();
        fs::write(hop.join(".zshenv"), ZSH_ZSHENV).unwrap();
        fs::write(hop.join(".zshrc"), zsh_zshrc()).unwrap();
        hop
    }

    #[cfg(test)]
    fn have_zsh() -> bool {
        std::process::Command::new("zsh")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    #[test]
    fn zsh_hop_preserves_user_default_zdotdir_and_installs_osc133() {
        // Regression guard: a non-exporting user (no ZDOTDIR in env) must
        // see ZDOTDIR *unset* when their own .zshrc runs, so a modular
        // ${ZDOTDIR:-$HOME/.config/zsh} idiom resolves to the user default and
        // their config loads - byte-identical to a plain terminal. The old hop
        // force-set ZDOTDIR=$HOME first, silently killing the whole config.
        if !have_zsh() {
            eprintln!("skipping zsh_hop_preserves_user_default_zdotdir: zsh absent");
            return;
        }
        let hop = write_hop_zdotdir("default");
        let home = std::env::temp_dir().join(format!("fno-test-home-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(home.join(".config/zsh")).unwrap();
        fs::write(
            home.join(".zshrc"),
            "ZDOTDIR=\"${ZDOTDIR:-$HOME/.config/zsh}\"\n\
             [ -r \"$ZDOTDIR/aliases.zsh\" ] && source \"$ZDOTDIR/aliases.zsh\"\n",
        )
        .unwrap();
        fs::write(
            home.join(".config/zsh/aliases.zsh"),
            "alias fno_marker='ok'\n",
        )
        .unwrap();

        let out = std::process::Command::new("zsh")
            .arg("-ic")
            .arg("alias fno_marker; typeset -f _fno_osc133_precmd >/dev/null && echo PRECMD_OK; echo ZD=$ZDOTDIR")
            .env_remove("USER_ZDOTDIR") // non-exporting user: the hop leaves USER_ZDOTDIR unset
            .env("ZDOTDIR", &hop)
            .env("HOME", &home)
            .output()
            .expect("spawn zsh");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let _ = fs::remove_dir_all(&hop);
        let _ = fs::remove_dir_all(&home);

        // AC1-HP: the alias loaded (the :- default fired) and ZDOTDIR ended at the user default.
        assert!(
            stdout.contains("fno_marker=ok"),
            "alias missing (ZDOTDIR clobbered): {stdout}"
        );
        assert!(
            stdout.contains(&format!("ZD={}/.config/zsh", home.display())),
            "ZDOTDIR not the user default: {stdout}"
        );
        // AC2-HP: OSC 133 hooks installed - the snippet ran last, not skipped by the fix.
        assert!(
            stdout.contains("PRECMD_OK"),
            "OSC 133 precmd hook missing: {stdout}"
        );
    }

    #[test]
    fn zsh_hop_no_user_rc_opens_bare_shell_with_osc133() {
        // AC1-EDGE: a $HOME with no .zshrc opens a working bare zsh with OSC 133
        // installed and prints no rc error.
        if !have_zsh() {
            eprintln!("skipping zsh_hop_no_user_rc: zsh absent");
            return;
        }
        let hop = write_hop_zdotdir("norc");
        let home = std::env::temp_dir().join(format!("fno-test-home-norc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap(); // no .zshrc, no .zshenv

        let out = std::process::Command::new("zsh")
            .arg("-ic")
            .arg("typeset -f _fno_osc133_precmd >/dev/null && echo PRECMD_OK")
            .env_remove("USER_ZDOTDIR")
            .env("ZDOTDIR", &hop)
            .env("HOME", &home)
            .output()
            .expect("spawn zsh");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let _ = fs::remove_dir_all(&hop);
        let _ = fs::remove_dir_all(&home);

        assert!(
            stdout.contains("PRECMD_OK"),
            "OSC 133 hook missing on bare shell: {stdout}"
        );
        assert!(
            stderr.is_empty(),
            "temp rc printed an error on the no-user-rc path: {stderr}"
        );
    }

    #[tokio::test]
    async fn server_spine_unspawnable_shell_falls_back() {
        let _gate = pty_gate_async().await;
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let (exit_tx, _exit_rx) = tokio::sync::mpsc::channel(4);
        let candidates = shell_candidates(Some(OsStr::new("/nonexistent/definitely-not-a-shell")));
        let shell = PtyShell::spawn(&candidates, 24, 80, None, "main", 7, tx, exit_tx)
            .expect("fallback must spawn");
        assert!(shell.is_child_alive());
        // Prove the fallback shell is real: round-trip a command, and assert
        // every chunk carries this pane's tag.
        shell.write_input(b"echo fallback-ok\r").unwrap();
        let mut seen = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await {
                Ok(Some((pane_id, chunk))) => {
                    assert_eq!(pane_id, 7, "reader must tag output with its pane id");
                    seen.extend_from_slice(&chunk);
                    if String::from_utf8_lossy(&seen).contains("fallback-ok") {
                        return;
                    }
                }
                Ok(None) => break,
                Err(_) => {}
            }
        }
        panic!(
            "fallback shell never echoed: {:?}",
            String::from_utf8_lossy(&seen)
        );
    }

    #[tokio::test]
    async fn server_spine_child_exit_signals_the_exit_channel() {
        let _gate = pty_gate_async().await;
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let (exit_tx, mut exit_rx) = tokio::sync::mpsc::channel(4);
        let shell = PtyShell::spawn(
            &shell_candidates(Some(OsStr::new("/bin/sh"))),
            24,
            80,
            None,
            "main",
            42,
            tx,
            exit_tx,
        )
        .expect("spawns");
        shell.write_input(b"exit\r").unwrap();
        let pid = tokio::time::timeout(std::time::Duration::from_secs(10), exit_rx.recv())
            .await
            .expect("exit must be signaled")
            .expect("channel open");
        assert_eq!(pid, 42, "exit signal must carry the pane id");
    }

    #[tokio::test]
    async fn server_spine_pane_env_carries_session_and_pane_id() {
        // 4a-G2: FNO_PANE joins FNO_SESSION in every pane child env, so a
        // hosted agent can name its own pane and an in-pane spawn inherits
        // the session. FNO_PANE_EPOCH rides along, non-empty, so the
        // turn-marker hook's first-writer pin has a per-spawn freshness key.
        let _gate = pty_gate_async().await;
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let (exit_tx, _exit_rx) = tokio::sync::mpsc::channel(4);
        let shell = PtyShell::spawn(
            &shell_candidates(Some(OsStr::new("/bin/sh"))),
            24,
            80,
            None,
            "envtest",
            31,
            tx,
            exit_tx,
        )
        .expect("spawns");
        // A non-empty epoch prints as `epoch-<digits>-epochend`; an unset one
        // would collapse to `epoch--epochend`, which the assertion rejects.
        shell
            .write_input(b"echo mark-$FNO_SESSION-$FNO_PANE-end epoch-$FNO_PANE_EPOCH-epochend\r")
            .unwrap();
        let mut seen = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await {
                Ok(Some((_, chunk))) => {
                    seen.extend_from_slice(&chunk);
                    let text = String::from_utf8_lossy(&seen);
                    if text.contains("mark-envtest-31-end") && text.contains("-epochend") {
                        assert!(
                            !text.contains("epoch--epochend"),
                            "FNO_PANE_EPOCH must be non-empty: {text:?}"
                        );
                        return;
                    }
                }
                Ok(None) => break,
                Err(_) => {}
            }
        }
        panic!(
            "pane env vars never echoed: {:?}",
            String::from_utf8_lossy(&seen)
        );
    }

    #[test]
    fn server_spine_all_candidates_unspawnable_is_a_clear_error() {
        let _gate = pty_gate();
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let (exit_tx, _exit_rx) = tokio::sync::mpsc::channel(4);
        let err = PtyShell::spawn(
            &["/nonexistent/a".into(), "/nonexistent/b".into()],
            24,
            80,
            None,
            "main",
            0,
            tx,
            exit_tx,
        )
        .err()
        .expect("must fail");
        let msg = err.to_string();
        assert!(msg.contains("/nonexistent/a"), "{msg}");
        assert!(msg.contains("/nonexistent/b"), "{msg}");
    }

    #[test]
    fn open_pty_times_out_instead_of_blocking() {
        let _gate = pty_gate();
        std::env::set_var("FNO_MUX_OPENPTY_TIMEOUT_MS", "200");
        set_hang_injection(std::time::Duration::from_secs(60));
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let (exit_tx, _exit_rx) = tokio::sync::mpsc::channel(4);
        let start = std::time::Instant::now();
        let err = PtyShell::spawn(
            &shell_candidates(Some(OsStr::new("/bin/sh"))),
            24,
            80,
            None,
            "main",
            0,
            tx,
            exit_tx,
        )
        .err()
        .expect("a wedged openpty must refuse the pane");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "must not block past the deadline: {:?}",
            start.elapsed()
        );
        let msg = err.to_string();
        assert!(msg.contains("openpty"), "{msg}");
        assert!(msg.contains("ls /dev"), "{msg}");
    }

    #[test]
    fn open_pty_timeout_covers_the_explicit_argv_path() {
        let _gate = pty_gate();
        std::env::set_var("FNO_MUX_OPENPTY_TIMEOUT_MS", "200");
        set_hang_injection(std::time::Duration::from_secs(60));
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let (exit_tx, _exit_rx) = tokio::sync::mpsc::channel(4);
        let start = std::time::Instant::now();
        let err = PtyShell::spawn_cmd(
            &["/bin/sh".to_string()],
            24,
            80,
            None,
            "main",
            0,
            tx,
            exit_tx,
        )
        .err()
        .expect("a wedged openpty must refuse spawn_cmd too");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "must not block past the deadline: {:?}",
            start.elapsed()
        );
        let msg = err.to_string();
        assert!(msg.contains("openpty"), "{msg}");
        assert!(msg.contains("ls /dev"), "{msg}");
    }

    #[test]
    fn open_pty_cooldown_skips_the_second_attempt() {
        let _gate = pty_gate();
        std::env::set_var("FNO_MUX_OPENPTY_TIMEOUT_MS", "200");
        set_hang_injection(std::time::Duration::from_secs(60));
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let (exit_tx, _exit_rx) = tokio::sync::mpsc::channel(4);
        PtyShell::spawn(
            &shell_candidates(Some(OsStr::new("/bin/sh"))),
            24,
            80,
            None,
            "main",
            0,
            tx,
            exit_tx,
        )
        .err()
        .expect("first spawn must time out and arm the cooldown");

        let (tx2, _rx2) = tokio::sync::mpsc::channel(64);
        let (exit_tx2, _exit_rx2) = tokio::sync::mpsc::channel(4);
        let start = std::time::Instant::now();
        let err = PtyShell::spawn(
            &shell_candidates(Some(OsStr::new("/bin/sh"))),
            24,
            80,
            None,
            "main",
            1,
            tx2,
            exit_tx2,
        )
        .err()
        .expect("second spawn must refuse immediately, not retry the real call");
        assert!(
            start.elapsed() < std::time::Duration::from_millis(50),
            "cooldown refusal must be near-instant, not pay the deadline again: {:?}",
            start.elapsed()
        );
        let msg = err.to_string();
        assert!(msg.contains("skipped"), "{msg}");
        assert!(msg.contains("ls /dev"), "{msg}");
    }

    #[test]
    fn open_pty_gives_up_after_too_many_timeouts_on_one_thread() {
        // Falsifier for the leak the cooldown alone does not close: a host
        // that stays wedged past every cooldown window must still leak only
        // a bounded number of threads on a given caller thread, not one per
        // retry forever. Seeds TOTAL_TIMEOUTS to one below the threshold
        // instead of driving MAX_TIMEOUTS_BEFORE_GIVING_UP real timeouts -
        // an earlier version of this test spawned real threads all the way
        // to the bound, which is exactly the pattern that produced the
        // process-wide counter-underflow bug this design replaced.
        let _gate = pty_gate();
        std::env::set_var("FNO_MUX_OPENPTY_TIMEOUT_MS", "50");
        set_hang_injection(std::time::Duration::from_secs(60));
        set_total_timeouts(MAX_TIMEOUTS_BEFORE_GIVING_UP - 1);

        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let (exit_tx, _exit_rx) = tokio::sync::mpsc::channel(4);
        let err = PtyShell::spawn(
            &shell_candidates(Some(OsStr::new("/bin/sh"))),
            24,
            80,
            None,
            "main",
            0,
            tx,
            exit_tx,
        )
        .err()
        .expect("one short of the threshold must still pay the deadline and time out");
        assert!(
            matches!(err, PtyError::OpenPtyTimeout(_)),
            "must still attempt, not give up early: {err}"
        );

        // Now at the threshold. Bypass the cooldown the call above just
        // armed (it would otherwise intercept first) to isolate the give-up
        // check itself: it must refuse immediately, without spawning
        // another probe.
        clear_wedge_cooldown();
        let start = std::time::Instant::now();
        let (tx2, _rx2) = tokio::sync::mpsc::channel(64);
        let (exit_tx2, _exit_rx2) = tokio::sync::mpsc::channel(4);
        let err = PtyShell::spawn(
            &shell_candidates(Some(OsStr::new("/bin/sh"))),
            24,
            80,
            None,
            "main",
            1,
            tx2,
            exit_tx2,
        )
        .err()
        .expect("at the threshold, must refuse instead of spawning again");
        assert!(
            start.elapsed() < std::time::Duration::from_millis(50),
            "must refuse immediately, not pay the deadline again: {:?}",
            start.elapsed()
        );
        assert!(
            matches!(err, PtyError::OpenPtyGivenUp(n) if n == MAX_TIMEOUTS_BEFORE_GIVING_UP),
            "must report the threshold reached, got: {err}"
        );
        let msg = err.to_string();
        assert!(msg.contains("openpty"), "{msg}");
        assert!(msg.contains("ls /dev"), "{msg}");
        std::env::remove_var("FNO_MUX_OPENPTY_TIMEOUT_MS");
    }

    #[test]
    fn open_pty_success_resets_the_giveup_counter() {
        // Falsifier for the reset-on-success fix: without it, TOTAL_TIMEOUTS
        // accumulates across the thread's whole lifetime instead of one
        // sustained incident, so a host that recovers between separate
        // wedge episodes would eventually and permanently refuse every
        // later pane, healthy host or not, until the process is restarted.
        let _gate = pty_gate();
        std::env::set_var("FNO_MUX_OPENPTY_TIMEOUT_MS", "50");
        set_total_timeouts(MAX_TIMEOUTS_BEFORE_GIVING_UP - 1);

        // A healthy open (no hang injected) must succeed and reset the
        // near-threshold count to 0, as if this were a fresh thread that
        // had never seen a timeout.
        set_hang_injection(std::time::Duration::ZERO);
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let (exit_tx, _exit_rx) = tokio::sync::mpsc::channel(4);
        PtyShell::spawn(
            &shell_candidates(Some(OsStr::new("/bin/sh"))),
            24,
            80,
            None,
            "main",
            0,
            tx,
            exit_tx,
        )
        .expect("a healthy host must still spawn one attempt below the threshold");

        // Two fresh wedge attempts. Without the reset, the pre-success
        // count (one below the threshold) survives the success, so the
        // FIRST of these alone would cross the threshold and the SECOND
        // would refuse immediately instead of paying the deadline again.
        set_hang_injection(std::time::Duration::from_secs(60));
        for i in 1..=2u64 {
            clear_wedge_cooldown();
            let (tx, _rx) = tokio::sync::mpsc::channel(64);
            let (exit_tx, _exit_rx) = tokio::sync::mpsc::channel(4);
            let err = PtyShell::spawn(
                &shell_candidates(Some(OsStr::new("/bin/sh"))),
                24,
                80,
                None,
                "main",
                i,
                tx,
                exit_tx,
            )
            .err()
            .expect("host is wedged again, this attempt must time out");
            assert!(
                matches!(err, PtyError::OpenPtyTimeout(_)),
                "attempt {i} must still pay the deadline, not read a stale near-threshold count: {err}"
            );
        }
        std::env::remove_var("FNO_MUX_OPENPTY_TIMEOUT_MS");
    }

    #[tokio::test]
    async fn healthy_open_pty_is_unchanged() {
        let _gate = pty_gate_async().await;
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let (exit_tx, _exit_rx) = tokio::sync::mpsc::channel(4);
        let shell = PtyShell::spawn(
            &shell_candidates(Some(OsStr::new("/bin/sh"))),
            24,
            80,
            None,
            "main",
            0,
            tx,
            exit_tx,
        )
        .expect("a healthy host must spawn exactly as before the offload");
        assert!(shell.is_child_alive());
    }
}
