//! Shared helpers for the review-coverage integration tests. A directory
//! module, not a test target of its own.

use std::fs;
use std::path::{Path, PathBuf};

/// Write an executable stub script. Probe-exec until the script actually runs
/// (ETXTBSY guard, same as tests/loop_check.rs): a parallel test's fork can
/// inherit the just-written fd, so an exec of this script can read as NotFound
/// and flip a gh-probed decision to advisory mode.
pub fn make_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();
    for _ in 0..100 {
        match std::process::Command::new(&path)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
        {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            _ => break,
        }
    }
    path
}
