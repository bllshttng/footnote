//! One subprocess read under a wall-clock budget (moved out of daemon.rs,
//! : the file-budget ratchet made daemon.rs shrink-only).

/// The most a child's pipe may contribute to memory. Past it the reader
/// drains to EOF without keeping, so a runaway child can never grow this
/// process; the overflow is named on stderr once and the truncated output
/// fails the caller's parse loudly.
const PIPE_KEEP_BYTES: usize = 64 * 1024 * 1024;

fn keep_capped<R: std::io::Read>(pipe: Option<R>, name: &str) -> Vec<u8> {
    let mut kept: Vec<u8> = Vec::new();
    let Some(mut pipe) = pipe else {
        return kept;
    };
    let mut chunk = [0u8; 65536];
    let mut named = false;
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) | Err(_) => return kept,
            Ok(n) => {
                if kept.len() < PIPE_KEEP_BYTES {
                    let room = PIPE_KEEP_BYTES - kept.len();
                    kept.extend_from_slice(&chunk[..n.min(room)]);
                } else if !named {
                    named = true;
                    eprintln!(
                        "bounded_cmd: {name} passed the {} byte keep cap; output truncated",
                        PIPE_KEEP_BYTES
                    );
                }
            }
        }
    }
}

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
        let out = keep_capped(stdout, "stdout");
        let err = keep_capped(stderr, "stderr");
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
/// The load-scaled wall budget for one subprocess read: `floor_s` on an
/// idle machine, doubling per 2 jobs per core of load, capped at `cap_s`.
/// A fork-starved machine needs minutes of wall clock for the same
/// subprocess chain; a fixed floor there SIGKILLs every read.
pub(crate) fn load_scaled_budget_s(floor_s: u64, cap_s: u64) -> u64 {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get() as f64)
        .unwrap_or(1.0);
    load_scaled_budget_for_s(
        crate::machine_sample::load_average().map(|(one, _, _)| one / cores),
        floor_s,
        cap_s,
    )
}

/// The pure shape, load handed in so a test drives it.
pub(crate) fn load_scaled_budget_for_s(
    load_per_core: Option<f64>,
    floor_s: u64,
    cap_s: u64,
) -> u64 {
    match load_per_core {
        Some(load) => ((floor_s as f64 * (load / 2.0).max(1.0)) as u64).clamp(floor_s, cap_s),
        None => floor_s,
    }
}

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

/// One subprocess read under a wall-clock budget, feeding `input` on the
/// child's stdin. The null-stdin default above stays for every existing
/// caller; the LLM one-shot seam feeds the prompt this way.
pub(crate) fn output_with_timeout_stdin(
    mut cmd: std::process::Command,
    secs: u64,
    input: &str,
) -> std::io::Result<std::process::Output> {
    use std::io::Write as _;
    use std::os::unix::process::CommandExt;
    let mut child = cmd
        .process_group(0)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    if let Some(mut pin) = child.stdin.take() {
        let _ = pin.write_all(input.as_bytes());
        let _ = pin.shutdown();
    }
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let reader = std::thread::spawn(move || {
        let out = keep_capped(stdout, "stdout");
        let err = keep_capped(stderr, "stderr");
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
