//! What the OS said when asked for a pid's create time, and whether that
//! answer proves a claim holder is running. Named by the question it
//! answers; agents_view.rs is over the shrink-only budget, so code that
//! lands here never grows it. Focused copy of `claims.rs::PidProbe`.

/// `Refused` is its own arm because the holder EXISTS when inspection is
/// denied - permission cannot be denied on a pid that is gone - so a
/// refusal must never read as death.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PidProbe {
    Created(i64),
    Absent,
    Refused,
}

#[cfg(target_os = "macos")]
pub(super) fn probe_pid(pid: i32) -> PidProbe {
    use std::mem;
    if pid <= 0 {
        return PidProbe::Absent;
    }
    let mut info: libc::proc_bsdinfo = unsafe { mem::zeroed() };
    let size = mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // proc_pidinfo's failure return is not uniformly -1, so clear errno and
    // consult it on ANY failed fill; reading a stale errno could honor EPERM
    // from an unrelated earlier call.
    unsafe { *libc::__error() = 0 };
    let written = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if written == size {
        return PidProbe::Created(
            (info.pbi_start_tvsec as i64) * 1000 + (info.pbi_start_tvusec as i64) / 1000,
        );
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::EPERM) | Some(libc::EACCES) => PidProbe::Refused,
        _ => PidProbe::Absent,
    }
}

#[cfg(target_os = "linux")]
pub(super) fn probe_pid(pid: i32) -> PidProbe {
    if pid <= 0 {
        return PidProbe::Absent;
    }
    let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return PidProbe::Refused,
        Err(_) => return PidProbe::Absent,
    };
    let Some(after) = stat.rsplit_once(')').map(|(_, tail)| tail) else {
        return PidProbe::Absent;
    };
    let Some(Ok(starttime)) = after.split_whitespace().nth(19).map(|v| v.parse::<i64>()) else {
        return PidProbe::Absent;
    };
    static BTIME: std::sync::OnceLock<Option<i64>> = std::sync::OnceLock::new();
    let Some(btime) = *BTIME.get_or_init(|| {
        let stat = std::fs::read_to_string("/proc/stat").ok()?;
        stat.lines()
            .find_map(|l| l.strip_prefix("btime ").and_then(|r| r.trim().parse().ok()))
    }) else {
        return PidProbe::Absent;
    };
    let tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if tck <= 0 {
        return PidProbe::Absent;
    }
    PidProbe::Created(btime * 1000 + starttime * 1000 / tck as i64)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn probe_pid(_pid: i32) -> PidProbe {
    PidProbe::Absent
}

/// Refused still reads live: the holder provably exists, and the events
/// recency window bounds what a badge may claim.
pub(super) fn probe_is_live(probe: PidProbe, acquired_at: i64) -> bool {
    match probe {
        PidProbe::Created(create_ms) => create_ms <= acquired_at,
        PidProbe::Refused => true,
        PidProbe::Absent => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{probe_is_live, PidProbe};

    #[test]
    fn probe_is_live_refused_reads_live_absent_dead() {
        // x-3735: inspection-refused is not pid-absent. A refusal proves the
        // pid EXISTS, so it must never read as death; only a real pid-reuse
        // (started after acquired_at) or a genuine absence reads dead. Driven
        // through the pure seam because no portable test pid refuses
        // inspection.
        assert!(probe_is_live(PidProbe::Refused, 0));
        assert!(probe_is_live(PidProbe::Created(100), 100));
        assert!(!probe_is_live(PidProbe::Created(101), 100));
        assert!(!probe_is_live(PidProbe::Absent, i64::MAX));
    }
}
