//! Per-process liveness probes: the start-time token and the zombie flag.
//! One read per platform; consumers compare the token only for equality
//! and read the zombie flag to prune lock holders that cannot hold a
//! flock anymore.

/// A live process's start time, used to distinguish "our worker" from a recycled
/// PID. `None` if the process is gone or the lookup is
/// unsupported/failed. The value is a per-host, per-boot quantity compared only
/// for equality against a value captured for the SAME pid, so the differing
/// units across platforms (Linux ticks vs macOS microseconds) do not matter.
///
/// DO NOT read this as a wall clock. At least three writers fill the column it
/// lands in, in at least three conventions: [`process_bsd`] (Linux ticks /
/// macOS micros), `_process_start_time` in cli/src/fno/agents/spawn_gate.py, and
/// `claude_adopt.rs`, which passes through whatever claude's own roster wrote.
/// Converting one of them to epoch time makes the equality comparisons in
/// `pid_is_ours` and `_pid_alive` fail across writers, which reaps live workers.
/// A consumer that needs a real start time needs its own field, not this token.
///
/// The zombie flag is ticket-liveness vocabulary: a zombie reads alive to
/// `pid_is_alive` (kill(pid,0) answers 0) but closed its fds at exit, so it
/// can hold neither a flock nor a lock ticket.
#[cfg(target_os = "linux")]
pub fn process_bsd(pid: u32) -> Option<(u64, bool)> {
    // /proc/<pid>/stat field 22 (1-based) is `starttime` in clock ticks since
    // boot. The comm field (2) can contain spaces and parens, so split on the
    // LAST ')' and index from there. After "comm)" the space-separated fields
    // are [state, ppid, ...]: state (zombie = 'Z') is index 0, starttime the
    // 20th (0-based index 19).
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = stat.rsplit_once(')')?.1;
    let mut fields = after.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let start = fields.nth(18)?.parse::<u64>().ok()?;
    Some((start, state == 'Z'))
}

/// macOS: `proc_pidinfo(PROC_PIDTBSDINFO)` fills a `proc_bsdinfo` whose
/// `pbi_start_tvsec`/`pbi_start_tvusec` is the process start time (folded to
/// microseconds) and whose `pbi_status == SZOMB` marks a zombie.
/// (`kinfo_proc` is not exposed by the libc crate.)
#[cfg(target_os = "macos")]
pub fn process_bsd(pid: u32) -> Option<(u64, bool)> {
    use std::mem;
    let mut info: libc::proc_bsdinfo = unsafe { mem::zeroed() };
    let size = mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: buffer is a zeroed proc_bsdinfo of exactly `size` bytes.
    // proc_pidinfo returns the number of bytes written; anything other than a
    // full struct means the process is gone / not introspectable -> None.
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
    Some((
        info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec,
        info.pbi_status == libc::SZOMB,
    ))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn process_bsd(_pid: u32) -> Option<(u64, bool)> {
    None
}

/// The start-time token [`process_bsd`] reads: per-host, per-boot, compared
/// only for equality (never a wall clock - see that function's contract).
pub fn process_start_time(pid: u32) -> Option<u64> {
    process_bsd(pid).map(|(start, _)| start)
}
