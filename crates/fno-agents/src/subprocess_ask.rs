//! Shared one-shot-subprocess primitives for the client-side `ask` ports
//! (codex + gemini). Extracted from `codex_ask.rs` (ab-73da4ac2) so the SIGINT
//! forwarding, process-group kill, grace reap, watchdog, and output tee live in
//! ONE place and the PR #371/#372 hardening carveouts apply to every provider:
//!
//! - cv-cfdb7a56 (SIGINT forwarding, ab-e7fdbcb6): forward operator Ctrl-C to
//!   the child's process group so codex/gemini + their sandbox descendants are
//!   not orphaned. Both providers' one-shot subprocess is `setpgid(0,0)` into
//!   its own group, so terminal SIGINT never reaches it without forwarding.
//! - cv-16eb2200 (canonicalize warn): [`resolve_ask_cwd`] warns at the failure
//!   point instead of silently joining cwd/current_dir.
//!
//! The two providers diverge ONLY in how they read stdout (codex parses a
//! per-line JSONL stream; gemini reads a single JSON blob) and how they treat
//! stderr (codex merges it, gemini drains it on a separate thread to keep the
//! JSON parse pure). Everything below is identical for both.

use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

// ===========================================================================
// SIGINT forwarding (ab-e7fdbcb6 / cv-cfdb7a56) — shared across providers
// ===========================================================================
//
// The one-shot ask subprocess (codex or gemini) runs in its OWN process group
// (`setpgid(0, 0)` in the spawner's pre_exec), so a terminal Ctrl-C — delivered
// by the tty to the fno foreground process group — never reaches the child or
// the sandbox subshells it spawns. Rust installs no SIGINT handler by default,
// so the parent fno process dies on the first Ctrl-C and ORPHANS the child.
//
// Python's providers catch `KeyboardInterrupt` mid-read and `os.killpg(pgid,
// SIGINT)`, then wait/escalate. We reproduce it with a process-global SIGINT
// handler installed only for the lifetime of one subprocess (the RAII guard
// below). The handler forwards SIGINT to the child group and records the
// interrupt; the read loop then ends naturally when the child tears down and
// closes its pipe, and the caller returns its `Interrupted` error (exit 130,
// matching CPython's KeyboardInterrupt exit).
//
// Only `libc::killpg` and atomic stores run inside the handler — both are
// async-signal-safe.

/// Process group of the in-flight ask child (0 when none). Read by the signal
/// handler; set/cleared by [`SigintForwarder`].
static ASK_CHILD_PGID: AtomicI32 = AtomicI32::new(0);
/// Set by the signal handler when a SIGINT was forwarded. Polled post-loop via
/// [`ask_interrupted`].
static ASK_INTERRUPTED: AtomicBool = AtomicBool::new(false);
/// Serializes installation + lifetime of the process-global SIGINT handler.
/// The fno ask client is one-shot, but tests run dispatch on parallel threads
/// in one binary; holding this lock for the guard's whole lifetime makes the
/// "one ask child in flight at a time" invariant *enforced* rather than merely
/// assumed, and prevents concurrent installs from clobbering the statics.
static SIGINT_MUTEX: Mutex<()> = Mutex::new(());

/// True iff the in-flight (or just-finished) child received a forwarded SIGINT.
/// Cleared on the next [`SigintForwarder::install`].
pub fn ask_interrupted() -> bool {
    ASK_INTERRUPTED.load(Ordering::SeqCst)
}

/// SIGINT handler: forward the signal to the ask child's process group and flag
/// the interrupt. Async-signal-safe (killpg + atomic store only).
extern "C" fn forward_sigint_to_child(_sig: libc::c_int) {
    let pgid = ASK_CHILD_PGID.load(Ordering::SeqCst);
    if pgid > 0 {
        // SAFETY: killpg is async-signal-safe; pgid is the child's group id.
        unsafe {
            libc::killpg(pgid, libc::SIGINT);
        }
    }
    ASK_INTERRUPTED.store(true, Ordering::SeqCst);
}

/// RAII guard installing the SIGINT-forwarding handler for the duration of one
/// ask subprocess, restoring the previous disposition (and clearing the pgid)
/// on drop. The fno ask client is one-shot, so only one ask child is ever in
/// flight per process.
#[must_use = "dropping the guard immediately uninstalls the SIGINT handler"]
pub struct SigintForwarder {
    prev: libc::sighandler_t,
    /// Held for the guard's whole lifetime so installs serialize (see
    /// `SIGINT_MUTEX`). Dropped after `Drop::drop` runs, i.e. after the prior
    /// disposition is restored and `ASK_CHILD_PGID` is cleared.
    _guard: MutexGuard<'static, ()>,
}

impl SigintForwarder {
    /// Install the handler, pointing it at `pgid` (== child pid, since the
    /// child is `setpgid(0, 0)` into its own group).
    pub fn install(pgid: u32) -> Self {
        // Serialize install + lifetime. Poisoning is irrelevant for a unit
        // lock; recover the guard either way.
        let guard = SIGINT_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        // With the lock held, no other forwarder can be live; Drop clears
        // ASK_CHILD_PGID before releasing the lock, so this always holds.
        debug_assert_eq!(
            ASK_CHILD_PGID.load(Ordering::SeqCst),
            0,
            "SIGINT_MUTEX held but ASK_CHILD_PGID != 0; guard lifetime invariant violated"
        );
        ASK_INTERRUPTED.store(false, Ordering::SeqCst);
        ASK_CHILD_PGID.store(pgid as i32, Ordering::SeqCst);
        // SAFETY: forward_sigint_to_child is async-signal-safe; we save the
        // prior disposition to restore on drop.
        let prev = unsafe {
            libc::signal(
                libc::SIGINT,
                forward_sigint_to_child as *const () as libc::sighandler_t,
            )
        };
        if prev == libc::SIG_IGN {
            // The parent explicitly ignored SIGINT (backgrounded / under a
            // supervisor). Standard Unix convention is to inherit that: undo
            // our handler immediately and do NOT forward. ASK_CHILD_PGID is
            // cleared so the handler (now SIG_IGN again) can never fire, and
            // Drop's generic `restore = self.prev` re-applies SIG_IGN.
            // SAFETY: restoring the just-saved SIG_IGN disposition.
            unsafe {
                libc::signal(libc::SIGINT, libc::SIG_IGN);
            }
            ASK_CHILD_PGID.store(0, Ordering::SeqCst);
        } else if prev == libc::SIG_ERR {
            // Installing a SIGINT handler effectively never fails, but if it
            // did we must NOT later restore SIG_ERR (itself an error). Warn and
            // let Drop fall back to SIG_DFL.
            eprintln!(
                "fno-agents: failed to install SIGINT handler; Ctrl-C will not forward to the child"
            );
        }
        Self {
            prev,
            _guard: guard,
        }
    }
}

impl Drop for SigintForwarder {
    fn drop(&mut self) {
        // Restore the prior disposition. For SIG_IGN we already re-applied it in
        // install (and never installed our handler); for SIG_ERR fall back to
        // SIG_DFL (restoring SIG_ERR is itself an error). Otherwise restore the
        // saved handler.
        let restore = if self.prev == libc::SIG_ERR {
            libc::SIG_DFL
        } else {
            self.prev
        };
        // SAFETY: restore the prior SIGINT disposition and clear the pgid so a
        // late signal can't target an exited (possibly recycled) pid.
        unsafe {
            libc::signal(libc::SIGINT, restore);
        }
        ASK_CHILD_PGID.store(0, Ordering::SeqCst);
    }
}

// ===========================================================================
// Process-group kill + grace reap
// ===========================================================================

/// Send `sig` to the process group of `pid`.
pub fn kill_pgrp(pid: u32, sig: libc::c_int) {
    unsafe {
        let pgid = libc::getpgid(pid as libc::pid_t);
        if pgid > 0 {
            libc::killpg(pgid, sig);
        }
    }
}

/// Reap `child`: wait up to `grace_sec`, then SIGTERM, then SIGKILL after 5s.
/// Returns `(exit_code, sigkill_escalated)`. (std has no `wait_timeout`, so we
/// spin-poll `try_wait` at 25ms.)
pub fn wait_with_grace(pid: u32, child: &mut Child, grace_sec: f64) -> (i32, bool) {
    let deadline = Instant::now() + Duration::from_secs_f64(grace_sec);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return (status.code().unwrap_or(-1), false),
            Ok(None) => {
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => break,
        }
    }
    // Grace expired: SIGTERM to pgrp.
    kill_pgrp(pid, libc::SIGTERM);
    let sigterm_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return (status.code().unwrap_or(-1), false),
            Ok(None) => {
                if Instant::now() >= sigterm_deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => break,
        }
    }
    // SIGKILL escalation.
    kill_pgrp(pid, libc::SIGKILL);
    let sigkill_deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return (status.code().unwrap_or(-1), true),
            Ok(None) => {
                if Instant::now() >= sigkill_deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => break,
        }
    }
    // Last resort: child not reaped after SIGKILL+2s.
    (-9, true)
}

// ===========================================================================
// output.jsonl tee
// ===========================================================================

/// Open the JSONL tee in append mode, creating parent dirs.
///
/// `Path::parent()` returns `Some("")` for a bare-filename relative path (e.g.
/// `"output.jsonl"`), and `create_dir_all("")` fails — skip the mkdir when the
/// parent is empty (the file lives in cwd and the dir already exists). Returns
/// the raw `io::Error` so each provider wraps it in its own `TeeOpen` variant
/// with a provider-tagged message.
pub fn open_tee(log_path: &Path) -> std::io::Result<std::fs::File> {
    if let Some(parent) = log_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
}

// ===========================================================================
// Wall-clock watchdog
// ===========================================================================

/// Cancelable wall-clock watchdog: on timeout, SIGTERM the child's process
/// group; escalate to SIGKILL after a 2s grace. Cancelable via an internal
/// channel so a happy-path completion (the caller calls [`AskWatchdog::cancel`]
/// before reaping) makes `recv_timeout` return `Disconnected` and the kill
/// cascade is skipped — mirrors Python's `for t in timers: t.cancel()` in the
/// `finally` block.
///
/// Python parity: the providers only arm the watchdog when `timeout > 0`. A
/// zero-duration timeout means "disabled" (caller opted out), NOT "immediate
/// expiry"; `Some(Duration::ZERO)` is treated as `None`.
pub struct AskWatchdog {
    timed_out: std::sync::Arc<AtomicBool>,
    done_tx: Option<Sender<()>>,
    handle: Option<JoinHandle<()>>,
}

impl AskWatchdog {
    /// Arm a watchdog for `pid` (its process group). `timeout == None` or
    /// `Some(ZERO)` arms nothing.
    pub fn spawn(pid: u32, timeout: Option<Duration>) -> Self {
        let timed_out = std::sync::Arc::new(AtomicBool::new(false));
        let watchdog_timeout = timeout.filter(|d| !d.is_zero());
        let (done_tx, done_rx) = channel::<()>();
        let handle = watchdog_timeout.map(|d| {
            let pid_for_wd = pid;
            let timed_out_for_wd = timed_out.clone();
            std::thread::spawn(move || {
                use std::sync::mpsc::RecvTimeoutError;
                match done_rx.recv_timeout(d) {
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                        // Main thread completed (or dropped its sender). Skip
                        // the kill cascade; the child either exited or is being
                        // reaped by `wait_with_grace` momentarily.
                        return;
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        timed_out_for_wd.store(true, Ordering::SeqCst);
                        kill_pgrp(pid_for_wd, libc::SIGTERM);
                    }
                }
                // Second-stage escalation: also cancelable. If the SIGTERM was
                // honored and main signals done within 2s, skip SIGKILL.
                match done_rx.recv_timeout(Duration::from_secs(2)) {
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => {}
                    Err(RecvTimeoutError::Timeout) => {
                        kill_pgrp(pid_for_wd, libc::SIGKILL);
                    }
                }
            })
        });
        Self {
            timed_out,
            done_tx: Some(done_tx),
            handle,
        }
    }

    /// Cancel the kill cascade (drop the sender). Call BEFORE reaping so a slow
    /// reap doesn't run out the watchdog's `recv_timeout` window.
    pub fn cancel(&mut self) {
        self.done_tx.take();
    }

    /// Join the watchdog thread so its forensic state (the `timed_out` store)
    /// is committed before [`AskWatchdog::timed_out`] is read. Call AFTER
    /// reaping.
    pub fn join(&mut self) {
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }

    /// Whether the watchdog fired (the child exceeded its wall-clock budget).
    ///
    /// Call [`AskWatchdog::join`] FIRST: the watchdog thread stores the flag
    /// just after `recv_timeout` returns `Timeout`, so a read before the thread
    /// is joined can race and observe a stale `false`. Both `run_codex` and
    /// `run_gemini` reap → `join()` → `timed_out()` in that order.
    pub fn timed_out(&self) -> bool {
        self.timed_out.load(Ordering::SeqCst)
    }
}

// ===========================================================================
// cwd resolution for the client `maybe_run_*_ask` hooks (cv-16eb2200)
// ===========================================================================

/// Resolve the `--cwd` param to an absolute path before it reaches the registry
/// row, mirroring Python's `Path(cwd).resolve()`.
///
/// cv-16eb2200: the prior code silently joined cwd/current_dir on a
/// `canonicalize` failure (a not-yet-existing path), producing a confusing
/// downstream error with no breadcrumb. Now the fallback warns at the failure
/// point so the operator can see why the recorded cwd diverged from `--cwd`.
pub fn resolve_ask_cwd(cwd_param: Option<&str>) -> PathBuf {
    match cwd_param {
        Some(c) => match std::fs::canonicalize(c) {
            Ok(p) => p,
            Err(e) => {
                let p = PathBuf::from(c);
                let resolved = if p.is_absolute() {
                    p
                } else {
                    std::env::current_dir().map(|d| d.join(&p)).unwrap_or(p)
                };
                eprintln!(
                    "fno-agents: could not canonicalize --cwd {:?} ({}); recording {:?}",
                    c, e, resolved
                );
                resolved
            }
        },
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    }
}
