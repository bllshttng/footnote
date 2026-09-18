//! Shared fixtures for the claims tests, moved verbatim out of `claims.rs`
//! for file budget: test motion is the sanctioned shrink.

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

/// Scrub the ambient session-pid stamp pair (`FNO_SESSION_PID` +
/// `FNO_SESSION_HARNESS`) so the durable-pid resolver walks (or answers
/// None) instead of reading a stamp inherited from the runner's env.
/// Callers hold test_env_lock; restore with `restore_session_pid_stamps`.
pub(super) fn scrub_session_pid_stamps() -> Vec<(&'static str, Option<std::ffi::OsString>)> {
    ["FNO_SESSION_PID", "FNO_SESSION_HARNESS"]
        .iter()
        .map(|v| {
            let saved = std::env::var_os(v);
            std::env::remove_var(v);
            (*v, saved)
        })
        .collect()
}

/// Point the stamp pair at `durable_pid` after scrubbing: the pair is the
/// deterministic seam that replaced the `FNO_BIN` stub.
pub(super) fn stamp_session_pid(
    durable_pid: u32,
) -> Vec<(&'static str, Option<std::ffi::OsString>)> {
    let saved = scrub_session_pid_stamps();
    std::env::set_var("FNO_SESSION_PID", durable_pid.to_string());
    std::env::set_var("FNO_SESSION_HARNESS", "claude");
    saved
}

pub(super) fn restore_session_pid_stamps(saved: Vec<(&'static str, Option<std::ffi::OsString>)>) {
    for (var, val) in saved {
        match val {
            Some(x) => std::env::set_var(var, x),
            None => std::env::remove_var(var),
        }
    }
}
