//! Shared fixtures for the claims tests, moved verbatim out of `claims.rs`
//! for file budget: test motion is the sanctioned shrink.

use super::*;
use tempfile::TempDir;

/// Pin FNO_AGENTS_HOME for a test whose renew call consults the session
/// witness (renew classifies through the registry leg), so an
/// ambient session id never reads the operator's real registry. Callers
/// hold test_env_lock; restore with `restore_agents_home`.
pub(super) fn pin_agents_home(td: &TempDir) -> Option<std::ffi::OsString> {
    let home = td.path().join("agents-home");
    std::fs::create_dir_all(&home).unwrap();
    let saved = std::env::var_os("FNO_AGENTS_HOME");
    std::env::set_var("FNO_AGENTS_HOME", &home);
    saved
}

pub(super) fn restore_agents_home(saved: Option<std::ffi::OsString>) {
    match saved {
        Some(v) => std::env::set_var("FNO_AGENTS_HOME", v),
        None => std::env::remove_var("FNO_AGENTS_HOME"),
    }
}

/// Scrub the ambient harness markers so `acquire` stamps no session id and
/// the renewal verdict never consults the session witness: no registry
/// read, no transcript probe, no latency under a parallel test load. The
/// cargo test binary runs inside a live claude session, so the vendor
/// markers are set. Callers hold test_env_lock.
pub(super) fn scrub_session_markers() -> Vec<(&'static str, Option<std::ffi::OsString>)> {
    const VARS: [&str; 4] = [
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_SESSION_ID",
        "FNO_HARNESS_SESSION_ID",
        "FNO_HARNESS_NAME",
    ];
    VARS.iter()
        .map(|v| {
            let saved = std::env::var_os(v);
            std::env::remove_var(v);
            (*v, saved)
        })
        .collect()
}

pub(super) fn restore_session_markers(saved: Vec<(&'static str, Option<std::ffi::OsString>)>) {
    for (v, val) in saved {
        match val {
            Some(x) => std::env::set_var(v, x),
            None => std::env::remove_var(v),
        }
    }
}

/// A pid the OS does not report, so `is_live` reads the claim as a corpse.
pub(super) fn dead_pid() -> u32 {
    let mut candidate = 999_999u32;
    while std::path::Path::new(&format!("/proc/{candidate}")).exists()
        || unsafe { libc::kill(candidate as i32, 0) } == 0
    {
        candidate += 1;
    }
    candidate
}

/// Point `FNO_BIN` at a stub answering `claim session-pid` with `pid`.
/// An empty `pid` reproduces the no-harness-ancestor degrade, which the
/// real verb signals with empty stdout and exit 0.
pub(super) fn stub_session_pid(dir: &std::path::Path, pid: &str) -> PathBuf {
    let script = dir.join("fno-stub");
    std::fs::write(&script, format!("#!/bin/sh\nprintf '%s' '{pid}'\n")).unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&script, perms).unwrap();
    script
}
