//! The test-owner identity the fno-agents-worker watchdog reads from
//! FNO_TEST_OWNER_PID/FNO_TEST_OWNER_BIRTH (x-7447): a test-spawned server
//! passes it to every keeper it launches, so the keeper reaps when the test
//! run ends instead of orphaning as a ppid-1 pane keeper for hours.
//!
//! This is the boundary-forced twin of fno-agents' `test_run::self_owner_env`
//! + `daemon::process_start_time`: the product boundary (the mux never links
//! the runtime, not even as a dev dependency - proven by
//! `the_mux_never_links_the_runtime`) forbids calling the original, so this
//! twin must stay behavior-identical to it. The UNITS are load-bearing: the
//! worker compares this value against its own `process_start_time` read, and
//! a mismatch reads as a dead owner that reaps the pane instantly.
//!
//! Shared by two compilation units through one source: the lib's cfg(test)
//! build (`src/server_tests.rs`) and the integration harness
//! (`tests/common/mod.rs` includes it by `#[path]`).

/// Microseconds-since-epoch on macOS, clock-ticks-since-boot on Linux -
/// exactly the representation `fno-agents::daemon::process_start_time`
/// returns, because the worker's `owner_alive` compares equality against it.
pub(crate) fn process_start_time(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        // /proc/<pid>/stat field 22 (1-based) is `starttime` in clock ticks
        // since boot; split on the LAST ')' because comm can hold spaces.
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let after = stat.rsplit_once(')')?.1;
        return after.split_whitespace().nth(19)?.parse::<u64>().ok();
    }
    #[cfg(target_os = "macos")]
    {
        use std::mem;
        let mut info: libc::proc_bsdinfo = unsafe { mem::zeroed() };
        let size = mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: buffer is a zeroed proc_bsdinfo of exactly `size` bytes;
        // proc_pidinfo only fills and never retains the pointer.
        let written = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                size,
            )
        };
        if written != size {
            return None;
        }
        return Some(info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec);
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

/// The current test process's identity in the env shape every spawned
/// server, client, and keeper inherits.
pub(crate) fn self_owner_env() -> [(&'static str, String); 2] {
    let pid = std::process::id();
    let birth =
        process_start_time(pid).expect("the current test process must have a readable birth time");
    [
        ("FNO_TEST_OWNER_PID", pid.to_string()),
        ("FNO_TEST_OWNER_BIRTH", birth.to_string()),
    ]
}

#[cfg(test)]
mod tests {
    use super::process_start_time;

    #[test]
    fn reads_a_live_pids_birth_and_none_for_a_bare_pid() {
        // Positive control: THIS process is introspectable on macOS/Linux.
        let own = process_start_time(std::process::id());
        assert!(own.is_some(), "own birth time must read");
        // pid 4 (launchd/init sibling slot, never us) is absent or refused on
        // both platforms; either way it must not fabricate a birth.
        let low = process_start_time(4);
        assert_ne!(low, own, "a foreign low pid must not read as our birth");
    }
}
