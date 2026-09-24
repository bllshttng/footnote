//! One subprocess read under a wall-clock budget (moved out of daemon.rs,
//! : the file-budget ratchet made daemon.rs shrink-only).

/// One subprocess read under a wall-clock budget: `std` has no
/// `Command::output` timeout, and a git stalled on a wedged filesystem must
/// not park the daemon's rm handler forever. Past the deadline the child's
/// process group is killed and the killed status returned, so a "kept"
/// receipt can never be contradicted by a removal finishing in the
/// background. The group kill matters: a child that forked a grandchild
/// would otherwise stay alive holding the piped stdout and park the read
/// past its bound. Spawn and wait failures carry as `Err`, for callers that
/// must tell "the binary is gone" apart from "it ran and was killed".
pub(crate) fn output_with_timeout_result(
    mut cmd: std::process::Command,
    secs: u64,
) -> std::io::Result<std::process::Output> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    let mut child = cmd
        .process_group(0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let mut err = Vec::new();
        if let Some(mut s) = stdout {
            let _ = s.read_to_end(&mut out);
        }
        if let Some(mut s) = stderr {
            let _ = s.read_to_end(&mut err);
        }
        (out, err)
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Ok(None) => {
                // The child is its own group leader (process_group(0)), so
                // this reaches the grandchildren a forking child left behind.
                unsafe {
                    libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
                }
                break child.wait()?;
            }
            Err(e) => return Err(e),
        }
    };
    let (stdout, stderr) = reader
        .join()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "output reader panicked"))?;
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

/// The Option form: every failure - a missing binary, a wait error, a
/// panicked reader - reads the same `None`, which the existing callers
/// already treat as "no receipt".
pub(crate) fn output_with_timeout(
    cmd: std::process::Command,
    secs: u64,
) -> Option<std::process::Output> {
    output_with_timeout_result(cmd, secs).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kill_bounds_a_bash_sleeper() {
        let dir = tempfile::tempdir().unwrap();
        let stub = crate::write_exec_stub(dir.path(), "s", "#!/bin/bash\nexec sleep 30\n");
        let started = std::time::Instant::now();
        let out = output_with_timeout_result(std::process::Command::new(&stub), 1)
            .expect("bash stub must spawn");
        let elapsed = started.elapsed();
        assert!(!out.status.success(), "killed child must read failed");
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "elapsed {elapsed:?}"
        );
    }

    #[test]
    fn kill_bounds_a_forking_sleeper() {
        let dir = tempfile::tempdir().unwrap();
        // No exec: sleep is a grandchild holding the piped stdout. The group
        // kill must still return the read inside its bound.
        let stub = crate::write_exec_stub(dir.path(), "s", "#!/bin/bash\nsleep 30\n");
        let started = std::time::Instant::now();
        let out = output_with_timeout_result(std::process::Command::new(&stub), 1)
            .expect("bash stub must spawn");
        let elapsed = started.elapsed();
        assert!(!out.status.success(), "killed child must read failed");
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "elapsed {elapsed:?}"
        );
    }

    #[test]
    fn missing_binary_is_an_err_not_none() {
        let cmd = std::process::Command::new("/nonexistent/fno-binary-for-tests");
        let err = output_with_timeout_result(cmd, 5).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }
}
