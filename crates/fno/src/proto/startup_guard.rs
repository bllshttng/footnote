//! The startup-marker ownership handshake: who holds `sock.start` from bind
//! until server exit, whether that holder is alive, and how a racing starter
//! retries when the holder exits mid-acquire. A child module of proto so the
//! shrink-only budget on proto.rs stays honest.
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{
    parse_pid_sidecar, pid_confirmed_dead, pid_is_zombie, pid_start_time, probe_status,
    ProbeOutcome,
};

/// How long a starter waits out a live marker before refusing. Mirrors the
/// client's own spawn-connect budget: a wedged startup must fail loudly, not
/// block a launch forever.
const WAIT_STARTUP_DEADLINE: Duration = Duration::from_secs(10);
const WAIT_STARTUP_POLL: Duration = Duration::from_millis(50);

/// Marker held from socket bind until the server exits. It protects a freshly
/// bound listener that cannot answer the query probe yet, so a concurrent
/// starter cannot mistake that startup window for a stale socket.
pub fn startup_sidecar_path(socket: &Path) -> PathBuf {
    socket.with_extension("start")
}

pub fn remove_startup_guard(socket: &Path) {
    let _ = std::fs::remove_file(startup_sidecar_path(socket));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StartupGuard {
    Owned,
    ExistingLive,
}

fn startup_guard_live(pid: i32, recorded_start: Option<u64>) -> bool {
    if pid <= 1 || pid_confirmed_dead(pid) || pid_is_zombie(pid) {
        return false;
    }
    recorded_start.is_none_or(|start| pid_start_time(pid as u32) == Some(start))
}

/// Sequence stamp for tmp names: unique per attempt within a process, where
/// the clock's granularity is not. Two same-process racers that compute the
/// same nanosecond stamp otherwise share one tmp name, and the loser's
/// unconditional tmp cleanup then deletes the winner's tmp mid-create, whose
/// hard_link then fails ENOENT and escaped the bind race as an error.
static MARKER_TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn create_startup_marker(marker: &Path) -> std::io::Result<()> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seq = MARKER_TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = marker
        .file_name()
        .map(|value| value.to_string_lossy())
        .unwrap_or_else(|| std::borrow::Cow::Borrowed("mux-start"));
    let tmp = marker.with_file_name(format!(
        ".{name}.tmp.{}.{}.{}",
        std::process::id(),
        stamp,
        seq
    ));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp)?;
        let pid = std::process::id();
        let contents = match pid_start_time(pid) {
            Some(start) => format!("{pid}:{start}"),
            None => pid.to_string(),
        };
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        std::fs::hard_link(&tmp, marker)
    })();
    let _ = std::fs::remove_file(&tmp);
    result
}

/// Test seam, mirroring the FNO_E2E logging gate: when the env var is set,
/// a racer pauses AFTER its create fails AlreadyExists and BEFORE it reads
/// the marker, so a test can land the owner's exit inside that window
/// deterministically and prove the read's NotFound retries instead of
/// escaping. Unset in production, the read is a no-op.
fn hold_start_marker_for_tests() {
    if let Some(v) = std::env::var_os("FNO_TEST_MARKER_HOLD_MS") {
        if let Ok(ms) = v.to_string_lossy().parse::<u64>() {
            std::thread::sleep(Duration::from_millis(ms));
        }
    }
}

/// Test seam, the owner-side mirror: when the env var is set, a just-OWNED
/// marker pauses before bind_or_probe's bind, so a test can land the racer's
/// bind inside the owner's startup window and pin the racer's verdict against
/// a marker held by another starter. Unset in production, a no-op.
fn hold_owned_marker_for_tests() {
    if let Some(v) = std::env::var_os("FNO_TEST_OWNED_HOLD_MS") {
        if let Ok(ms) = v.to_string_lossy().parse::<u64>() {
            std::thread::sleep(Duration::from_millis(ms));
        }
    }
}

/// Remove the marker, tolerating only "already gone" (the owner's own exit
/// or a peer's reclaim). Every other error propagates: a remove that keeps
/// failing while the marker sits there would otherwise spin this loop hot
/// forever, where the pre-module code surfaced a named error.
fn remove_marker_gone_tolerant(marker: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(marker) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// The outcome of [`claim_startup`].
pub(crate) enum StartupClaim {
    /// We hold the marker: bind.
    Own,
    /// A live server answered while we waited: attach instead.
    Running,
}

/// Take the right to bind `socket`, waiting out a live starter instead of
/// racing its bind. Racing is the two-server shape: this starter binds first,
/// the marker owner resumes, loses its bind, reads the silent winner as a
/// stale socket, and unlinks a live peer. A waiter instead converges: the
/// winner answers (`Running`), its marker clears or dies and the claim is
/// retried (`Own`), or a wedged startup refuses at the deadline.
pub(crate) fn claim_startup(socket: &Path) -> std::io::Result<StartupClaim> {
    let deadline = Instant::now() + WAIT_STARTUP_DEADLINE;
    loop {
        match acquire_startup_guard(socket)? {
            StartupGuard::Owned => return Ok(StartupClaim::Own),
            StartupGuard::ExistingLive => {
                if probe_status(socket) == ProbeOutcome::Alive {
                    return Ok(StartupClaim::Running);
                }
                let marker = startup_sidecar_path(socket);
                let still_starting = match std::fs::read_to_string(&marker) {
                    Ok(raw) => parse_pid_sidecar(&raw)
                        .map(|(pid, start)| startup_guard_live(pid, start))
                        .unwrap_or(false),
                    Err(_) => false,
                };
                if still_starting {
                    if Instant::now() >= deadline {
                        return Err(startup_in_progress(socket));
                    }
                    std::thread::sleep(WAIT_STARTUP_POLL);
                    continue;
                }
                // Holder gone mid-startup: clear its marker and retry the
                // claim. A peer reclaiming the same stale marker may have
                // removed it first - gone is gone, the loop just retries.
                let _ = std::fs::remove_file(&marker);
                std::thread::sleep(WAIT_STARTUP_POLL);
            }
        }
    }
}

fn startup_in_progress(path: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::WouldBlock,
        format!("mux startup in progress at {}", path.display()),
    )
}

pub(crate) fn acquire_startup_guard(socket: &Path) -> std::io::Result<StartupGuard> {
    let marker = startup_sidecar_path(socket);
    loop {
        match create_startup_marker(&marker) {
            Ok(()) => {
                hold_owned_marker_for_tests();
                return Ok(StartupGuard::Owned);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // The marker's owner removes it at exit. Between this racer's
                // create-fails-AlreadyExists and the read below, that exit
                // can land and take the marker along: NotFound there is the
                // loop's signal to try owning the path itself, never an
                // error. A bind race over a serving server used to escape
                // bind_or_probe as Err(ENOENT) through exactly this seam.
                hold_start_marker_for_tests();
                let raw = match std::fs::read_to_string(&marker) {
                    Ok(raw) => raw,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(read) => {
                        return Err(std::io::Error::new(
                            read.kind(),
                            format!(
                                "cannot read mux startup marker {}: {read}",
                                marker.display()
                            ),
                        ))
                    }
                };
                let Some((pid, recorded_start)) = parse_pid_sidecar(&raw) else {
                    if !socket.exists() || matches!(probe_status(socket), ProbeOutcome::Dead) {
                        // A peer reclaiming the same stale marker may have
                        // removed it first; NotFound there is their success,
                        // and the loop simply retries owning the path.
                        remove_marker_gone_tolerant(&marker)?;
                        continue;
                    }
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("invalid mux startup marker {}", marker.display()),
                    ));
                };
                if startup_guard_live(pid, recorded_start) {
                    return Ok(StartupGuard::ExistingLive);
                }
                remove_marker_gone_tolerant(&marker)?;
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod reclaim_gone_tests {
    use super::super::{pid_sidecar_path, reclaim_unresponsive_holder};

    /// The fix under test: a pid sidecar that vanished between the caller's
    /// exists() gate and reclaim's read means the holder already exited, so
    /// reclaim must answer Ok (the no-holder takeover path) instead of
    /// escaping bind_or_probe as ENOENT.
    #[test]
    fn reclaim_with_no_pid_sidecar_is_ok_not_enoent() {
        let dir = std::env::temp_dir().join(format!("fno-reclaim-gone-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let gone = reclaim_unresponsive_holder(&dir.join("missing.sock"));
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(matches!(&gone, Ok(())), "gone holder must be Ok: {gone:?}");
    }

    /// Fail-closed is preserved: a sidecar that is PRESENT but unparseable
    /// still refuses takeover with InvalidData instead of signaling blind.
    #[test]
    fn reclaim_with_garbage_pid_sidecar_still_refuses() {
        let dir = std::env::temp_dir().join(format!("fno-reclaim-garbage-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("garbage.sock");
        std::fs::write(pid_sidecar_path(&sock), "not-a-pid").unwrap();
        let refused = reclaim_unresponsive_holder(&sock);
        std::fs::remove_dir_all(&dir).unwrap();
        match refused {
            Err(e) => assert_eq!(
                e.kind(),
                std::io::ErrorKind::InvalidData,
                "garbage sidecar must refuse, got {e}"
            ),
            other => panic!("garbage sidecar must not pass: {other:?}"),
        }
    }
}
