//! What proves a claude roster row dead.
//!
//! Named by the question it answers. The rm live gate, the reaper's stop
//! confirmation, and the merge cleanup all decide on these predicates, so
//! "what counts as dead" cannot diverge between them. The file-budget gate
//! holds daemon.rs at shrink-only; these moved here with the death-evidence
//! work that grew it.

use crate::state;

/// The row's harness row id: the stored short id, else the 8-char prefix of
/// the recorded harness session id.
pub(crate) fn claude_row_id(e: &state::RegistryEntry) -> Option<String> {
    if !e.short_id.is_empty() {
        return Some(e.short_id.clone());
    }
    e.harness_session_id
        .as_deref()
        .filter(|session_id| !session_id.is_empty())
        .map(|session_id| session_id.chars().take(8).collect())
}

/// True only when a KNOWN, warning-free roster snapshot was consulted and the
/// row is not in it. A `None`/unknown snapshot proves nothing, so it is never
/// absent on that basis alone; a list that carried warnings is PARTIAL, and a
/// row hidden among the skipped rows would read as absent here. The single
/// predicate both the pre-cascade live-gate and the cascade's own
/// already-absent check apply, so "what counts as absent" cannot diverge
/// between the two call sites.
pub(crate) fn claude_row_provably_absent(
    claude_agents: Option<&crate::claude_roster::ClaudeAgentsSnapshot>,
    row_id: Option<&str>,
) -> bool {
    claude_agents.is_some_and(|snap| {
        snap.is_known()
            && snap.warning_text().is_empty()
            && row_id.is_some_and(|id| snap.find(id).is_none())
    })
}

/// Existence-specific death probe: ESRCH is the one errno that means "no such
/// process". Every other answer is NOT death - EPERM is a live foreign-uid
/// process, and any other failure is a broken instrument. A broken instrument
/// must never read as a dead worker: `process_start_time`'s None conflates
/// gone with lookup-failed, so the reaper and rm must decide on this probe.
#[cfg(unix)]
pub(crate) fn pid_is_gone(pid: u32) -> bool {
    // kill(0, sig) and kill(-1, sig) are group/broadcast forms, never
    // existence probes; a pid above i32::MAX casts to a negative pid_t and
    // would read as one of those broadcasts. Only a positive, representable
    // pid may vote on death.
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// No kill(2) surface: the probe can never answer, and never-death holds.
#[cfg(not(unix))]
pub(crate) fn pid_is_gone(_pid: u32) -> bool {
    false
}
