//! The per-row liveness classifier: the pid probes and the status ladder.
//! The served window lives in crate::served_liveness. Child of
//! `agents_view`; parent items resolve through the glob.

use super::*;

/// A recorded pid is CONFIRMED gone (`kill(pid, 0)` -> ESRCH). Fails toward
/// "not confirmed" on any other outcome (alive, or unprobeable/EPERM): a
/// falsification must be positive, never inferred from an ambiguous errno.
/// pid 0/1 are never a worker's pid, so a stored 0/1 (corrupt row) also reads
/// as unconfirmed rather than as license to signal the caller's own group or
/// init.
fn pid_confirmed_dead(pid: u64) -> bool {
    if pid <= 1 || pid > i32::MAX as u64 {
        return false;
    }
    // SAFETY: signal 0 performs no delivery, only an existence/permission
    // check (mirrors `server.rs::pid_alive`, inverted for a POSITIVE read).
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// A recorded pid positively EXISTS: `kill(pid, 0)` succeeds, or fails with
/// EPERM (the process is there, just not ours). Only ESRCH is an absence, and
/// `pid_confirmed_dead` owns that answer.
fn pid_confirmed_alive(pid: u64) -> bool {
    if pid <= 1 || pid > i32::MAX as u64 {
        return false;
    }
    // SAFETY: signal 0 performs no delivery, only an existence/permission
    // check.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Derive [`Liveness`] for one row. `status` is the raw registry string;
/// `pid`/`short_id` are read the same tolerant way `derive_rows` reads every
/// other field. Pure and syscall-free except the `kill(pid, 0)` probe, gated
/// behind an orphaned, failed, or terminal status so a live-ish row never
/// pays it.
pub(super) fn derive_liveness(status: &str, pid: Option<u64>, short_id: &str) -> Liveness {
    if matches!(status, "orphaned" | "failed") {
        return match pid {
            Some(pid) if pid_confirmed_alive(pid) => Liveness::Alive,
            Some(pid) if pid_confirmed_dead(pid) => Liveness::Dead,
            _ => Liveness::Unmeasured,
        };
    }
    let terminal = matches!(status, "exited" | "permanent-dead" | "permanent_dead");
    if !terminal {
        return Liveness::Alive;
    }
    // Mirrors fno-agents' gc.rs::removal_is_corroborated (this crate does not
    // depend on fno-agents; the raw JSON reader restates the same rule): a
    // row with neither a pid nor a short_id owns no identity surface to
    // falsify, so the absence itself is the corroboration -- there is
    // nothing left that COULD be checked and found alive.
    let liveness_surface = pid.is_some() || !short_id.is_empty();
    let corroborated_dead = !liveness_surface || pid.is_some_and(pid_confirmed_dead);
    if corroborated_dead {
        Liveness::Dead
    } else {
        Liveness::Unmeasured
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_liveness_pure_function_covers_the_three_states() {
        // Non-terminal status: Alive regardless of pid/short_id.
        assert_eq!(derive_liveness("live", None, ""), Liveness::Alive);
        assert_eq!(derive_liveness("busy", Some(999_999), ""), Liveness::Alive);
        // Orphaned/failed: the recorded pid decides. A live pid is Alive, a
        // pid confirmed gone is Dead, and no pid stays Unmeasured.
        let this_process = std::process::id() as u64;
        assert_eq!(
            derive_liveness("orphaned", Some(this_process), "sid"),
            Liveness::Alive
        );
        assert_eq!(
            derive_liveness("orphaned", Some(0x7fff_fff0), "sid"),
            Liveness::Dead
        );
        assert_eq!(
            derive_liveness("orphaned", None, "sid"),
            Liveness::Unmeasured
        );
        assert_eq!(
            derive_liveness("failed", Some(this_process), "sid"),
            Liveness::Alive
        );
        // Terminal + no identity surface at all: nothing to falsify -> Dead
        // (mirrors gc.rs's !liveness_surface corroboration).
        assert_eq!(derive_liveness("exited", None, ""), Liveness::Dead);
        // Terminal + a pid that is unambiguously gone: a real pid this
        // process does not own. Use a pid in-range but astronomically
        // unlikely to be a live process on the test runner.
        assert_eq!(
            derive_liveness("exited", Some(0x7fff_fff0), ""),
            Liveness::Dead
        );
        // Terminal + THIS test process's own pid (definitely alive): the
        // status/pid contradiction that used to render a confident "x".
        let me = std::process::id() as u64;
        assert_eq!(
            derive_liveness("exited", Some(me), "short"),
            Liveness::Unmeasured
        );
        assert_eq!(
            derive_liveness("permanent-dead", Some(me), ""),
            Liveness::Unmeasured
        );
    }
}
