//! Quiet daemon retirement: which reason, if any, retires the daemon on an
//! idle tick, and what evidence a verdict must carry before anything exits.
//! Two reasons share one gate: plain idle-elapsed, and measured
//! build drift -- the on-disk binary changed under the running daemon. The
//! async serve loop in [`crate::daemon`] spawns the blocking liveness probe
//! and consumes the verdict these predicates grade.

use std::time::Instant;

use serde_json::json;

use crate::drift::DriftState;
use crate::paths::AgentsHome;

/// The idle-exit predicate: is any WORKER live on this home? A registry row is
/// not a reason to stay resident -- rows outlive their workers by design (the
/// GC reaps them a grace window later), so the registry-emptiness test this
/// replaced made idle-exit unsatisfiable on any machine that had ever spawned
/// a worker (: 78 daemons at once, all idle, all orphaned). The question
/// is whether a worker is LIVE, answered by the same pair `gc_sweep_impl`
/// uses: a live worker socket, or a pid that is still ours.
///
/// A MISSING registry answers `true`: a fresh machine has never tracked a
/// worker, and lazy-exit must hold there (the documented contract covers the
/// very first daemon). An EXISTING but unreadable registry answers `false`
/// (stay resident): that is an absence with two explanations, and exiting on a
/// transient read failure would trade a moment of caution for a fleet of dead
/// workers' supervisors.
///
/// A worker socket counts as live only if something ANSWERS on it, not if the
/// file exists: a worker killed by anything that did not reap its socket (the
/// confirmed-stop path is the only reaper) leaves a stale file behind, and on a
/// pid-less live row the GC cannot settle it - file-existence liveness would
/// then pin the daemon forever, one stale socket per home reinstating the
/// never-exits defect this function exists to close. `worker_socket_reachable`
/// is the same connect probe the stop path treats as the authoritative
/// PID-reuse-immune signal.
pub(crate) fn no_live_worker(home: &AgentsHome) -> bool {
    let path = home.registry_json();
    if !path.exists() {
        return true;
    }
    let socket_candidates = home.scan_worker_sockets();
    let socket_is_live = |short_id: &str| {
        socket_candidates.iter().any(|s| s == short_id)
            && std::os::unix::net::UnixStream::connect(home.worker_sock(short_id)).is_ok()
    };
    crate::state::load_registry(&path)
        .map(|r| {
            !r.entries.iter().any(|e| {
                socket_is_live(&e.short_id)
                    || e.pid
                        .map(|p| crate::daemon::pid_is_ours(p, e.pid_start_time))
                        .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// Which reason, if any, retires the daemon through the shared graceful tail
/// on this idle tick. Measured build drift outranks plain
/// idle-elapsed when both fire; both still need the fresh no-worker probe
/// verdict before anything exits. Fail-safe by construction: a missing
/// fingerprint or an unreadable exe classifies `Unknown`, which is never a
/// retirement, and a live active-backlog supervisor blocks both reasons.
pub(crate) fn quiet_retire_reason(
    drift: Option<&DriftState>,
    ab_active: bool,
    idle_elapsed: bool,
) -> Option<&'static str> {
    if ab_active {
        return None;
    }
    if matches!(drift, Some(DriftState::Drifted { .. })) {
        return Some("drift");
    }
    if idle_elapsed {
        return Some("idle");
    }
    None
}

/// The freshness gate a probe verdict must pass before it may retire the
/// daemon: no worker live, no request served, and no registry write while the
/// probe ran. Pane-substrate workers spawn by writing the registry directly
/// with no daemon contact, so the mtime is the one positive marker of that
/// race (shared verbatim by idle and drift retirement).
pub(crate) fn probe_verdict_fresh(
    no_worker: bool,
    probe_activity: Instant,
    last_activity: Instant,
    probe_mtime: Option<std::time::SystemTime>,
    mtime_now: Option<std::time::SystemTime>,
) -> bool {
    no_worker && probe_activity == last_activity && mtime_now == probe_mtime
}

/// The final `daemon_exited` payload. Every exit path flows through
/// one tail, and before this it emitted `clean: true` unconditionally, so the
/// socket-lost retirement - where something unlinked and rebound our socket
/// path - logged identically to a graceful SIGTERM shutdown. A watchdog
/// reading `daemon_exited` alone could not tell them apart; `clean` is false
/// only for that abnormal ending, and `reason` names which path fired.
pub(crate) fn daemon_exited_payload(reason: &str) -> serde_json::Value {
    json!({"clean": reason != "socket-lost", "reason": reason})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_fp(tag: &str, size: u64) -> crate::drift::ExeFingerprint {
        crate::drift::ExeFingerprint {
            path: std::path::PathBuf::from(format!("/bin/fno-agents-{tag}")),
            mtime_nanos: 1,
            size,
        }
    }

    fn drifted() -> DriftState {
        DriftState::Drifted {
            running: fake_fp("running", 10),
            on_disk: fake_fp("running", 12),
        }
    }

    /// AC1-HP: measured drift retires without waiting out the idle window.
    #[test]
    fn quiet_retire_reason_drift_fires_without_idle_wait() {
        assert_eq!(
            quiet_retire_reason(Some(&drifted()), false, false),
            Some("drift")
        );
    }

    /// When both conditions hold, drift is the named reason.
    #[test]
    fn quiet_retire_reason_drift_outranks_idle() {
        assert_eq!(
            quiet_retire_reason(Some(&drifted()), false, true),
            Some("drift")
        );
    }

    /// AC1-ERR: a missing fingerprint and an Unknown classification are never
    /// a retirement; only the ordinary idle path may fire.
    #[test]
    fn quiet_retire_reason_fails_safe_on_unknown_and_missing() {
        assert_eq!(quiet_retire_reason(None, false, false), None);
        assert_eq!(
            quiet_retire_reason(Some(&DriftState::Unknown), false, false),
            None
        );
        assert_eq!(
            quiet_retire_reason(Some(&DriftState::Unknown), false, true),
            Some("idle")
        );
    }

    /// A fresh build only ever retires through idle-elapsed.
    #[test]
    fn quiet_retire_reason_idle_when_fresh() {
        let fp = fake_fp("fresh", 10);
        assert_eq!(
            quiet_retire_reason(
                Some(&crate::drift::classify(Some(&fp), Some(&fp))),
                false,
                true
            ),
            Some("idle")
        );
        assert_eq!(
            quiet_retire_reason(
                Some(&crate::drift::classify(Some(&fp), Some(&fp))),
                false,
                false
            ),
            None
        );
    }

    /// AC1-ERR: a live active-backlog supervisor blocks drift and idle alike.
    #[test]
    fn quiet_retire_reason_ab_active_blocks_both() {
        assert_eq!(quiet_retire_reason(Some(&drifted()), true, false), None);
        assert_eq!(quiet_retire_reason(None, true, true), None);
    }

    /// The fresh-verdict gate: a retirement fires only when the probe saw no
    /// live worker AND nothing moved (no served request, no registry write)
    /// while it ran. AC1-ERR's live-worker and changed-registry refusals.
    #[test]
    fn probe_verdict_fresh_gates_on_worker_activity_and_registry() {
        let t = Instant::now();
        let m = std::time::SystemTime::now();
        // All quiet: fresh.
        assert!(probe_verdict_fresh(true, t, t, Some(m), Some(m)));
        // A live worker refuses.
        assert!(!probe_verdict_fresh(false, t, t, Some(m), Some(m)));
        // A request served during the probe refuses.
        assert!(!probe_verdict_fresh(
            true,
            t,
            Instant::now(),
            Some(m),
            Some(m)
        ));
        // A registry write during the probe refuses.
        assert!(!probe_verdict_fresh(
            true,
            t,
            t,
            Some(m),
            Some(m + std::time::Duration::from_secs(1))
        ));
        // A vanished or unreadable registry (None vs Some) refuses too.
        assert!(!probe_verdict_fresh(true, t, t, Some(m), None));
        assert!(!probe_verdict_fresh(true, t, t, None, Some(m)));
    }

    /// The drift exit flows the same clean tail as idle, with its own reason.
    #[test]
    fn daemon_exited_payload_marks_drift_clean() {
        let payload = daemon_exited_payload("drift");
        assert_eq!(payload["clean"], true);
        assert_eq!(payload["reason"], "drift");
    }
}
