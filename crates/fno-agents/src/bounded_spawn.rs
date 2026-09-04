//! The bounded transport's spawn half: one child spawn, retried while the
//! failure is transient.
//!
//! `run_bounded` owns the wait, the kill, and the pipe drains; this module
//! owns only getting a child to exist. The retry lives here because a spawn
//! failure is the one transport outcome that says nothing about the read
//! itself: on a loaded machine (a CI runner mid-suite, a fleet host near its
//! process limit) `fork` answers EAGAIN for a binary that is perfectly
//! spawnable a millisecond later. The gh probe already retried exactly this
//! class at its own layer; one red CI run surfaced the same failure one layer
//! down, where no retry existed, and a healthy `pr-heal` run exited "unreadable
//! world" instead of refusing on the branch it was asked about. Absence stays
//! immediate: ENOENT is a fact about the disk, not about the moment.

use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Stdio};

/// Attempts per spawn, and the pause between them: the same shape as the gh
/// probe's loop, which exists for the same reason (ETXTBSY clears in
/// milliseconds).
const SPAWN_ATTEMPTS: u32 = 3;
const RETRY_PAUSE: std::time::Duration = std::time::Duration::from_millis(5);

/// ENOENT is the one spawn error worth classifying as stable: it says the
/// binary is not there and will still not be there in 5ms, and it is the one
/// the shell fallback in `run_probe` has to see on attempt one. Everything
/// else retries. Most of that set really is moment-shaped (EAGAIN under fork
/// pressure, ETXTBSY while the binary is still being written, EMFILE at the
/// fd ceiling), but not all of it: EACCES on a mode-644 file is as stable as
/// ENOENT and still burns all three attempts. That is the deliberate trade.
/// Enumerating every stable errno is how one gets sorted into the wrong
/// bucket; the bound is what makes guessing wrong cheap - two extra forks and
/// 10ms on a binary that was never going to run.
fn failure_is_transient(kind: std::io::ErrorKind) -> bool {
    kind != std::io::ErrorKind::NotFound
}

/// Spawn `bin args` in `cwd` for the bounded transport: null stdin, piped
/// stdout/stderr, own process group, transient spawn failures retried. The
/// error is the LAST attempt's kind, so a stable failure surfaces as itself.
pub(crate) fn spawn_bounded(
    bin: &OsStr,
    args: &[&str],
    cwd: &Path,
) -> Result<std::process::Child, std::io::ErrorKind> {
    use std::os::unix::process::CommandExt;

    let mut attempt = 0;
    loop {
        let spawned = Command::new(bin)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn();
        match spawned {
            Ok(child) => return Ok(child),
            Err(e) if !failure_is_transient(e.kind()) => return Err(e.kind()),
            Err(e) => {
                attempt += 1;
                if attempt >= SPAWN_ATTEMPTS {
                    return Err(e.kind());
                }
                std::thread::sleep(RETRY_PAUSE);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absence_is_stable_and_never_retried() {
        assert!(!failure_is_transient(std::io::ErrorKind::NotFound));
    }

    #[test]
    fn a_loaded_machines_eagain_is_transient() {
        // EAGAIN maps to WouldBlock, the moment-shaped case. PermissionDenied
        // is the deliberate over-capture: EACCES is stable, and retrying it
        // anyway is what keeps this classifier a one-line rule.
        assert!(failure_is_transient(std::io::ErrorKind::WouldBlock));
        assert!(failure_is_transient(std::io::ErrorKind::PermissionDenied));
    }

    #[test]
    fn a_missing_binary_answers_not_found() {
        let err = spawn_bounded(
            OsStr::new("/nonexistent/bounded-spawn-binary"),
            &["x"],
            std::env::temp_dir().as_path(),
        )
        .err()
        .expect("a missing binary cannot spawn");
        assert_eq!(err, std::io::ErrorKind::NotFound);
    }

    #[test]
    fn a_stable_spawn_error_terminates_as_itself_after_the_bounded_retries() {
        // A regular file with no execute bit answers EACCES on every attempt,
        // so this proves the retry loop TERMINATES and reports the real kind
        // rather than hanging or synthesizing one.
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("not-executable");
        std::fs::write(&p, "#!/bin/sh\nexit 0\n").unwrap();
        let err = spawn_bounded(p.as_os_str(), &[], tmp.path())
            .err()
            .expect("a non-executable file cannot spawn");
        assert_eq!(err, std::io::ErrorKind::PermissionDenied);
    }
}
