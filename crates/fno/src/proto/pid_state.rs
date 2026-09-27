//! The zombie read: an exited, unreaped child is dead, not alive. Its one
//! consumer contract is `pid_confirmed_dead`'s reachable arm (and every
//! `|| pid_is_zombie` caller beside it). Extracted from proto.rs: the file
//! crossed the shrink-only line; test motion and this module are the
//! sanctioned shrink.

/// True while `pid` is a zombie: dead but not yet reaped by its parent, so
/// `kill(pid, 0)` keeps succeeding even though it holds no fds and serves
/// nothing. A bare-init container never reaps an adopted orphan, so waiting
/// out a grace window for ESRCH there never converges; a zombie must read
/// as gone the moment it is observed. An unreadable pid reads not-zombie:
/// this helper never invents a death.
///
/// On macOS the kernel record that answers for a zombie is the sysctl
/// `KERN_PROC_PID` read: proc_pidinfo's BSD-status read answers a zero
/// write for a zombie (measured 2026-09-18: PROC_PIDTBSDINFO writes 0
/// bytes for a `<defunct>` process), so it can never fire there. libc does
/// not export `kinfo_proc` on Apple targets, so the macOS arm pins the one
/// field it needs - the `extern_proc` prefix (p_un 16, two pointers 16,
/// p_flag 4) puts `p_stat` at byte 36 of the 648-byte record. The ABI is
/// stable on 64-bit Darwin.
#[cfg(target_os = "macos")]
pub fn pid_is_zombie(pid: i32) -> bool {
    #[repr(C, align(8))]
    struct KinfoProcScratch {
        head: KinfoProcHead,
        tail: [u8; 768],
    }
    #[repr(C)]
    struct KinfoProcHead {
        p_un: [u8; 16],
        p_vmspace: u64,
        p_sigacts: u64,
        p_flag: libc::c_int,
        p_stat: u8,
    }
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid];
    let mut info: KinfoProcScratch = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of::<KinfoProcScratch>();
    // SAFETY: sysctl fills a caller-owned zeroed buffer; mib and size live
    // in this frame and are read only during the call.
    let done = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            4,
            &mut info as *mut _ as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    done == 0 && info.head.p_stat == libc::SZOMB as u8
}

#[cfg(target_os = "linux")]
pub fn pid_is_zombie(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| Some(s.rsplit_once(')')?.1.trim_start().starts_with('Z')))
        .unwrap_or(false)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn pid_is_zombie(_pid: i32) -> bool {
    false
}
