//! One subprocess read under a wall-clock budget (moved out of daemon.rs,
//! x-a150: the file-budget ratchet made daemon.rs shrink-only).

/// One subprocess read under a wall-clock budget: `std` has no
/// `Command::output` timeout, and a git stalled on a wedged filesystem must
/// not park the daemon's rm handler forever. Past the deadline the child is
/// killed and the killed status returned, so a "kept" receipt can never be
/// contradicted by a removal finishing in the background.
pub(crate) fn output_with_timeout(
    mut cmd: std::process::Command,
    secs: u64,
) -> Option<std::process::Output> {
    use std::io::Read;
    let mut child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;
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
                let _ = child.kill();
                break child.wait().ok()?;
            }
            Err(_) => return None,
        }
    };
    let (stdout, stderr) = reader.join().ok()?;
    Some(std::process::Output {
        status,
        stdout,
        stderr,
    })
}
