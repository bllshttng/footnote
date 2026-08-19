//! Client side of the daemon protocol (Wave 3): lazy-start the daemon, connect,
//! and forward one request. Kept in the library so it is exercised by the
//! integration tests without shelling out to the compiled binary.

use crate::drift::{classify, DriftState, ExeFingerprint};
use crate::paths::AgentsHome;
use crate::protocol::{read_response, write_request, ProtocolError, Request, Response};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::net::UnixStream;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("protocol: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("daemon did not come up within {0:?}")]
    DaemonStartTimeout(Duration),
    #[error(
        "a daemon holds the supervisor singleton lock but did not answer within \
         {0:?} - it is alive and saturated, not absent"
    )]
    DaemonBusy(Duration),
    #[error("daemon exited during startup with status {0}")]
    DaemonExitedEarly(std::process::ExitStatus),
    #[error(
        "daemon binary not found: {0} - the fno-agents triad (client/daemon/worker) \
         is split here. Run `fno update` to redeploy the pair, or set \
         FNO_AGENTS_DAEMON_BIN to a coherent same-build daemon."
    )]
    DaemonBinMissing(PathBuf),
    #[error("daemon is not running")]
    DaemonNotRunning,
}

/// Resolve the daemon binary: `FNO_AGENTS_DAEMON_BIN` or a sibling of the
/// current executable named `fno-agents-daemon`.
pub fn resolve_daemon_bin() -> PathBuf {
    if let Some(v) = std::env::var_os("FNO_AGENTS_DAEMON_BIN") {
        return PathBuf::from(v);
    }
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("fno-agents-daemon")))
        .unwrap_or_else(|| PathBuf::from("fno-agents-daemon"))
}

/// True when no process currently holds the supervisor singleton lock, so
/// there is no daemon entitled to this socket and starting one is legitimate.
///
/// This is the positive marker a connect probe cannot be (x-ef7f): a busy
/// holder still holds the lock, while a busy holder answers no connect. We take
/// the lock only to ask the question and release it immediately -- the daemon
/// we spawn takes it for real, for its whole lifetime.
///
/// SHARED, not exclusive. An exclusive probe answers the same question but
/// blocks every other taker while it runs, which turns a read-only question
/// into interference: two clients probing at once make one of them see a
/// phantom incumbent, and a probe landing on a daemon's own acquisition makes
/// that daemon believe it lost a race nobody was running. A shared lock
/// conflicts with the daemon's exclusive hold, which is the signal we want, and
/// with nothing else. The daemon also retries its acquisition, so neither side
/// depends on the other being fast.
///
/// A lock error that is NOT `WouldBlock` (no flock support, an unwritable
/// state root) answers `true`. The daemon refuses to start at all without a
/// working flock (`flock_self_test`), so failing open here preserves today's
/// behavior and its error message rather than wedging every verb behind a
/// question the filesystem cannot answer.
fn daemon_lock_is_free(home: &AgentsHome) -> bool {
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(home.supervisor_lock())
    {
        Ok(f) => f,
        Err(_) => return true,
    };
    match file.try_lock_shared() {
        Ok(()) => true, // dropping `file` releases it on return
        Err(e) => {
            let io_err: std::io::Error = e.into();
            io_err.kind() != std::io::ErrorKind::WouldBlock
        }
    }
}

/// Poll `sock` until it answers or `budget` from `start` is spent.
async fn wait_for_socket(sock: &Path, start: Instant, budget: Duration) -> Result<(), ClientError> {
    while start.elapsed() < budget {
        if UnixStream::connect(sock).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Err(ClientError::DaemonStartTimeout(budget))
}

/// Outcome of waiting on an incumbent that holds the singleton lock.
enum Incumbent {
    /// It answered. Nothing to start.
    Serving,
    /// It released the lock without ever answering, so no daemon owns the
    /// socket now and starting one is legitimate again.
    Gone,
    /// It held the lock for the whole budget and never answered.
    Busy,
}

/// Wait for whoever holds the singleton lock, re-checking the lock each tick.
///
/// Polling the socket alone is not enough. `restart` SIGTERMs the daemon and
/// resumes the moment the socket stops answering, but the dying daemon still
/// holds its flock for the rest of `run()`. A client landing in that window
/// would otherwise wait the full budget for a socket nobody will ever bind, and
/// report a start that never happened. Re-reading the lock turns that into a
/// `Gone`, and the caller starts the daemon `restart` was asking for.
async fn wait_for_incumbent(
    home: &AgentsHome,
    sock: &Path,
    start: Instant,
    budget: Duration,
) -> Incumbent {
    while start.elapsed() < budget {
        if UnixStream::connect(sock).await.is_ok() {
            return Incumbent::Serving;
        }
        if daemon_lock_is_free(home) {
            return Incumbent::Gone;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Incumbent::Busy
}

/// Ensure a daemon is serving on `home`'s socket, lazy-starting one if not.
/// Returns once a connect succeeds.
///
/// A failed connect is not licence to fork. The comment that used to sit here
/// claimed the lazy-start race was resolved daemon-side by socket-bind
/// exclusivity, so a redundant fork simply lost and exited. It did not: the
/// daemon guarded the bind with its own connect probe, and a daemon too busy to
/// accept in time read as absent, so the challenger unlinked a LIVE listener and
/// bound a fresh inode. Every takeover added a supervisor instead of replacing
/// one, and the added load made the next probe likelier to misread -- 59 of them
/// at once, measured. `call()` reaches this from every client verb, so every
/// timed-out verb minted another.
///
/// Both halves are now gated on the same sidecar lockfile, which the daemon
/// holds for its entire process lifetime. Here that means: ask whether any
/// process owns the singleton before spawning a competitor for it, and when one
/// does, wait for the incumbent instead.
pub async fn ensure_daemon(
    home: &AgentsHome,
    daemon_bin: &std::path::Path,
) -> Result<(), ClientError> {
    let sock = home.supervisor_sock();
    if UnixStream::connect(&sock).await.is_ok() {
        return Ok(());
    }
    if !daemon_bin.exists() {
        return Err(ClientError::DaemonBinMissing(daemon_bin.to_path_buf()));
    }

    let start = Instant::now();
    let budget = Duration::from_secs(10);

    // A daemon owns the singleton and is merely slow to answer (booting, or
    // saturated). Wait it out. Spawning here is what compounded the wedge.
    if !daemon_lock_is_free(home) {
        match wait_for_incumbent(home, &sock, start, budget).await {
            Incumbent::Serving => return Ok(()),
            Incumbent::Busy => return Err(ClientError::DaemonBusy(budget)),
            // It let the lock go without binding. Fall through and start one.
            Incumbent::Gone => {}
        }
    }

    // A FRESH deadline for the start we are about to attempt. Waiting out an
    // incumbent that then let the lock go can consume most of the budget, and
    // charging that wait to a daemon we had not spawned yet left the poll loop
    // below with zero iterations -- `restart` reported a timeout for a start
    // that never got a single poll. Its `Gone` window is exactly where that
    // happens, since the outgoing daemon holds the flock for up to the
    // runtime's shutdown timeout past socket removal.
    let start = Instant::now();

    // The lock was free a moment ago, so no daemon is entitled to this socket.
    // We released it to let the child take it, which leaves a window where a
    // second client also finds it free and also spawns. That race is bounded
    // and self-correcting in a way the old one was not: the loser fails its
    // `try_lock` at bind time, returns `AlreadyRunning`, and exits WITHOUT
    // touching the socket -- one short-lived extra process, not a supervisor
    // that keeps running on an inode nobody can reach.
    eprintln!("(lazy-starting daemon)");
    // Detached, own process group: the daemon must outlive this client.
    let mut cmd = tokio::process::Command::new(daemon_bin);
    cmd.process_group(0);
    cmd.env("FNO_AGENTS_HOME", home.root());
    // Inherit the worker/daemon bin overrides so a test harness's binaries are used.
    if let Some(v) = std::env::var_os("FNO_AGENTS_WORKER_BIN") {
        cmd.env("FNO_AGENTS_WORKER_BIN", v);
    }
    let mut child = cmd.spawn()?;

    while start.elapsed() < budget {
        if UnixStream::connect(&sock).await.is_ok() {
            return Ok(());
        }
        // A daemon that REFUSED startup (x-4c87: a divergent registry exits
        // nonzero before binding, having printed the path and both counts to
        // inherited stderr) must fail this verb immediately with its real
        // cause, not burn the full socket budget on a socket that will never
        // appear (code-review on PR 924).
        if let Ok(Some(status)) = child.try_wait() {
            if !status.success() {
                return Err(ClientError::DaemonExitedEarly(status));
            }
            // Exit ZERO without a socket is the race loser's designed ending:
            // it found the lock held, deferred to the winner, and stopped. The
            // winner is coming up on this same socket right now, so failing the
            // verb here turns "one short-lived extra process" into a
            // user-visible error. Wait for the winner instead.
            //
            // Unless nobody holds the lock. Then no race was lost and nobody is
            // coming, so this was a real refusal that happened to exit 0 (an
            // immediate idle-exit, a mismatched binary). Report it now rather
            // than spend the whole budget on a socket that will never appear.
            if daemon_lock_is_free(home) {
                return Err(ClientError::DaemonExitedEarly(status));
            }
            return wait_for_socket(&sock, start, budget).await;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // Budget spent and the child still lives: it is genuinely slow to boot.
    // Drop the handle WITHOUT killing (tokio does not kill on drop; the child
    // is in its own process group) so it can finish booting and serve a later
    // verb, matching the old detach behavior. This leak used to be unbounded --
    // the next verb re-probed, timed out again, and forked again. It is bounded
    // now: the surviving child holds the singleton lock, so the next verb takes
    // the wait branch above instead of spawning a competitor for it.
    drop(child);
    Err(ClientError::DaemonStartTimeout(budget))
}

/// Send one request to the daemon (lazy-starting it first) and return the
/// response.
pub async fn call(
    home: &AgentsHome,
    daemon_bin: &std::path::Path,
    req: &Request,
) -> Result<Response, ClientError> {
    ensure_daemon(home, daemon_bin).await?;
    let mut conn = UnixStream::connect(home.supervisor_sock()).await?;
    write_request(&mut conn, req).await?;
    Ok(read_response(&mut conn).await?)
}

/// Send one request to an ALREADY-RUNNING daemon, WITHOUT lazy-starting one.
/// `status` uses this so it reports a down daemon (exit 13) rather than booting
/// one just to describe it as up (AC10-ERR).
pub async fn call_if_running(home: &AgentsHome, req: &Request) -> Result<Response, ClientError> {
    let mut conn = match UnixStream::connect(home.supervisor_sock()).await {
        Ok(c) => c,
        // Only "nothing is listening" means the daemon is down. A permission
        // error or a non-socket at the path is a real fault that must surface
        // rather than masquerade as exit-13 "daemon down" (Codex P2).
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Err(ClientError::DaemonNotRunning)
        }
        Err(e) => return Err(ClientError::Io(e)),
    };
    write_request(&mut conn, req).await?;
    Ok(read_response(&mut conn).await?)
}

// ---------------------------------------------------------------------------
// Binary-version drift detection + restart (ab-1891cdff).
// ---------------------------------------------------------------------------

/// Parse the daemon's reported running-exe fingerprint out of an `agent.status`
/// result. `None` when the daemon reported no fingerprint (a pre-drift daemon,
/// or a startup stat failure) -> the caller reads `Unknown`.
fn running_fingerprint(status: &Value) -> Option<ExeFingerprint> {
    let d = status.get("daemon")?;
    let path = d.get("exe_path")?.as_str()?;
    let mtime_nanos = d.get("exe_mtime")?.as_i64()?;
    let size = d.get("exe_size")?.as_u64()?;
    Some(ExeFingerprint {
        path: PathBuf::from(path),
        mtime_nanos,
        size,
    })
}

/// Classify drift from an already-fetched `agent.status` result: compare the
/// daemon's reported running fingerprint to a fresh stat of the binary this
/// client would launch now ([`resolve_daemon_bin`]). Synchronous and never
/// lazy-starts. Used by `status`, which already holds the status payload, to
/// avoid a second RPC.
pub fn drift_from_status(status: &Value) -> DriftState {
    let running = running_fingerprint(status);
    let on_disk = ExeFingerprint::of(&resolve_daemon_bin());
    classify(running.as_ref(), on_disk.as_ref())
}

/// Probe an already-running daemon for binary drift. Issues one `agent.status`
/// RPC and never lazy-starts: a down daemon is [`DriftState::DaemonDown`] (no
/// warning), not a reason to boot one. Any transport/parse error fails safe to
/// [`DriftState::Unknown`]. Used by `list`, which does not otherwise fetch the
/// daemon status.
pub async fn check_daemon_drift(home: &AgentsHome) -> DriftState {
    let req = Request::new(1, "agent.status", json!({}));
    match call_if_running(home, &req).await {
        Ok(resp) => match resp.result() {
            Some(result) => drift_from_status(result),
            None => DriftState::Unknown,
        },
        Err(ClientError::DaemonNotRunning) => DriftState::DaemonDown,
        Err(_) => DriftState::Unknown,
    }
}

/// The result of a successful daemon restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartOutcome {
    /// The pid of the daemon that was replaced, or `None` if none was running
    /// (the restart degraded to a fresh start).
    pub old_pid: Option<u32>,
    /// The pid of the freshly-started daemon now serving.
    pub new_pid: u32,
}

/// Why a restart could not complete. Each variant names the pid where relevant
/// so the `restart` verb can fail *loud* (Locked Decision: a failed restart must
/// never read as success).
#[derive(Debug, thiserror::Error)]
pub enum RestartError {
    #[error("SIGTERM to daemon pid {pid} failed: {reason}")]
    SigtermFailed { pid: u32, reason: String },
    #[error("daemon pid {pid} did not exit after SIGTERM within {secs}s; check it manually")]
    DidNotExit { pid: u32, secs: u64 },
    #[error("daemon status response missing daemon.pid")]
    StatusMissingPid,
    #[error(transparent)]
    Client(#[from] ClientError),
}

/// Bounded wait for the old daemon to release the supervisor socket. On a clean
/// SIGTERM the daemon unlinks its own socket, so a failed connect means cleared.
const RESTART_SOCKET_TIMEOUT: Duration = Duration::from_secs(5);

enum SigtermResult {
    /// SIGTERM delivered.
    Sent,
    /// The pid was already gone (ESRCH) -- treat as a successful "it exited".
    AlreadyGone,
    /// The signal could not be delivered (e.g. EPERM); a loud, named failure.
    Failed(String),
}

/// Send SIGTERM to `pid`, mapping the syscall result to a typed verdict. ESRCH
/// (no such process) is `AlreadyGone`, not a failure -- the daemon exiting on
/// its own between the status read and here is the success we wanted. Any other
/// errno is a `Failed` the caller surfaces loudly.
fn send_sigterm(pid: u32) -> SigtermResult {
    // SAFETY: kill(pid, SIGTERM) has no memory effects; errno is read only on the
    // error return.
    let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    if rc == 0 {
        return SigtermResult::Sent;
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(e) if e == libc::ESRCH => SigtermResult::AlreadyGone,
        _ => SigtermResult::Failed(err.to_string()),
    }
}

/// Read the serving daemon's pid via `agent.status` (already-running path).
async fn read_daemon_pid(home: &AgentsHome) -> Result<u32, RestartError> {
    let req = Request::new(1, "agent.status", json!({}));
    let resp = call_if_running(home, &req).await?;
    resp.result()
        .and_then(|r| r.get("daemon"))
        .and_then(|d| d.get("pid"))
        .and_then(Value::as_u64)
        .map(|p| p as u32)
        .ok_or(RestartError::StatusMissingPid)
}

/// True once nothing is listening on the supervisor socket (the old daemon
/// released it), bounded by `RESTART_SOCKET_TIMEOUT`.
async fn await_socket_clear(home: &AgentsHome) -> bool {
    let sock = home.supervisor_sock();
    let start = Instant::now();
    while start.elapsed() < RESTART_SOCKET_TIMEOUT {
        if UnixStream::connect(&sock).await.is_err() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    UnixStream::connect(&sock).await.is_err()
}

/// Lazy-start a fresh daemon and return its pid. Shared by every restart exit.
async fn start_fresh(home: &AgentsHome, daemon_bin: &Path) -> Result<u32, RestartError> {
    ensure_daemon(home, daemon_bin).await?;
    read_daemon_pid(home).await
}

/// Restart the daemon: SIGTERM the running one (graceful drain; PTY workers
/// survive -- Outcome B -- and are re-adopted by the fresh daemon's startup
/// recovery), wait for the socket to clear, then lazy-start a fresh daemon built
/// from the current binary. Reports `OLD -> NEW`.
///
/// - No daemon running -> fresh start, `old_pid = None` (idempotent).
/// - A SIGTERM failure (e.g. EPERM) or a daemon that will not exit within the
///   bound is a loud [`RestartError`], never a silent "restarted".
/// - The SIGTERM targets the daemon pid ONLY; it is pid-reuse-guarded by the
///   daemon's own start-time check before signalling, so a recycled pid is never
///   hit.
pub async fn restart_daemon(
    home: &AgentsHome,
    daemon_bin: &Path,
) -> Result<RestartOutcome, RestartError> {
    // Probe the running daemon for its pid + start time. A down daemon just
    // starts fresh.
    let status = Request::new(1, "agent.status", json!({}));
    let result = match call_if_running(home, &status).await {
        Ok(resp) => resp.result().cloned(),
        Err(ClientError::DaemonNotRunning) => None,
        Err(e) => return Err(RestartError::Client(e)),
    };

    let Some(result) = result else {
        let new_pid = start_fresh(home, daemon_bin).await?;
        return Ok(RestartOutcome {
            old_pid: None,
            new_pid,
        });
    };

    let daemon = result.get("daemon");
    let old_pid = daemon
        .and_then(|d| d.get("pid"))
        .and_then(Value::as_u64)
        .ok_or(RestartError::StatusMissingPid)? as u32;
    let recorded_start = daemon
        .and_then(|d| d.get("pid_start_time"))
        .and_then(Value::as_u64);

    // pid-reuse guard: only SIGTERM a pid that is still THIS daemon. If it is
    // already gone (or its pid was recycled), don't signal a stranger -- just
    // start fresh. We do NOT unlink the socket here: a stale socket file is
    // cleaned up race-free by the daemon's own bind_supervisor_socket
    // (exclusive flock, then remove + bind), and unlinking from the client could
    // delete a socket a concurrent client just rebound.
    if !crate::daemon::pid_is_ours(old_pid, recorded_start) {
        let new_pid = start_fresh(home, daemon_bin).await?;
        return Ok(RestartOutcome {
            old_pid: Some(old_pid),
            new_pid,
        });
    }

    match send_sigterm(old_pid) {
        SigtermResult::Sent | SigtermResult::AlreadyGone => {}
        SigtermResult::Failed(reason) => {
            return Err(RestartError::SigtermFailed {
                pid: old_pid,
                reason,
            })
        }
    }

    if !await_socket_clear(home).await {
        return Err(RestartError::DidNotExit {
            pid: old_pid,
            secs: RESTART_SOCKET_TIMEOUT.as_secs(),
        });
    }
    // Do NOT unlink the socket here (codex P2, PR #472): once the clear window
    // passes, a concurrent client could already have lazy-started and bound a
    // fresh daemon on this path; an unconditional remove would unlink that live
    // daemon's socket, leaving it unreachable while we start a second one. A
    // genuinely stale socket file (e.g. from a SIGKILL'd predecessor) is removed
    // race-free by the next daemon's bind_supervisor_socket. ensure_daemon below
    // connects first, so if a fresh daemon is already serving we adopt it rather
    // than starting a duplicate.
    let new_pid = start_fresh(home, daemon_bin).await?;
    Ok(RestartOutcome {
        old_pid: Some(old_pid),
        new_pid,
    })
}

#[cfg(test)]
mod drift_restart_tests {
    use super::*;

    #[test]
    fn drift_from_status_unknown_when_no_fingerprint() {
        // A status payload without exe_* fields (older daemon) -> Unknown, never
        // a false Drifted. (resolve_daemon_bin is irrelevant: a None running
        // fingerprint short-circuits to Unknown regardless of the on-disk side.)
        let status = json!({"daemon": {"pid": 1, "state": "serving"}});
        assert_eq!(drift_from_status(&status), DriftState::Unknown);
    }

    #[test]
    fn running_fingerprint_parses_and_rejects() {
        // The status-payload parser is the glue between the daemon's reported
        // fingerprint and the pure `classify`; the classification matrix itself is
        // tested in `crate::drift`. No env mutation, so this is parallel-safe.
        let status = json!({"daemon": {
            "exe_path": "/opt/fno-agents-daemon",
            "exe_mtime": 1_700_000_000_000_000_000_i64,
            "exe_size": 4242_u64,
        }});
        let fp = running_fingerprint(&status).expect("parses a full fingerprint");
        assert_eq!(fp.path, PathBuf::from("/opt/fno-agents-daemon"));
        assert_eq!(fp.mtime_nanos, 1_700_000_000_000_000_000);
        assert_eq!(fp.size, 4242);

        // A null/absent field (pre-drift daemon) -> None -> caller reads Unknown.
        let partial = json!({"daemon": {"exe_path": "/opt/x", "exe_size": 1_u64}});
        assert!(running_fingerprint(&partial).is_none());
        assert!(running_fingerprint(&json!({})).is_none());
    }

    #[test]
    fn send_sigterm_reports_already_gone_for_dead_pid() {
        // AC2-FR support: a reaped child's pid is ESRCH -> AlreadyGone (a
        // successful "it exited"), distinct from the loud Failed path.
        let child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let pid = child.id();
        let mut child = child;
        let _ = child.wait(); // reap so the pid is fully gone
        std::thread::sleep(Duration::from_millis(50));
        match send_sigterm(pid) {
            // pid retired -> ESRCH; not-yet-retired -> Sent. Both acceptable;
            // a Failed (e.g. EPERM) for our own just-reaped child is the bug.
            SigtermResult::AlreadyGone | SigtermResult::Sent => {}
            SigtermResult::Failed(e) => panic!("unexpected Failed: {e}"),
        }
    }
}
