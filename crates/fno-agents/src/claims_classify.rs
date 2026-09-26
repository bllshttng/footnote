//! The claim state ladder: is a held claim Live, Suspect, or Stale?
//!
//! Extracted from claims.rs so that file keeps shrinking under the file
//! budget; every item re-exports through `crate::claims`, so the public
//! paths are unchanged.

use super::{
    basis, is_same_machine, now_ms, pid_dies_with_session, probe_pid, ClaimRecord, ClaimState,
    PidProbe, SessionLiveness, SessionWitness, BLUEPRINT_HOLDER_PREFIX, BLUEPRINT_LEASE_MS,
    HOLDER_PROCESS, UNRESOLVED_GRACE_MS,
};

/// The liveness reading beside its cause (mirrors
/// `staleness._liveness_reading`). `probe` is injectable so tests and the
/// parity harness can drive the Refused arm deterministically; the Python
/// leg's equivalent seam is monkeypatching `_probe_create_time`.
fn liveness_reading(rec: &ClaimRecord, probe: &dyn Fn(i32) -> PidProbe) -> (bool, &'static str) {
    if rec.pid_unavailable {
        return (false, basis::PID_UNAVAILABLE);
    }
    if !is_same_machine(&rec.host, rec.machine_id.as_deref()) {
        return (false, basis::OFFHOST);
    }
    let Some(pid) = rec.pid else {
        return (false, basis::PID_UNAVAILABLE);
    };
    match probe(pid) {
        PidProbe::Created(create_ms) => {
            // PID reuse: the current occupant of the pid slot started AFTER
            // the claim was filed, so it is a different process.
            if create_ms > rec.acquired_at {
                (false, basis::PID_REUSE)
            } else {
                (true, basis::LIVE)
            }
        }
        PidProbe::Absent => (false, basis::PID_ABSENT),
        PidProbe::Refused => (false, basis::ACCESS_DENIED),
    }
}

/// Is the claim's holder verifiably running? (mirrors `staleness.is_live`)
/// False when: no pid was recorded, cross-machine, the pid is gone, the
/// holder refuses inspection, or the current occupant of the pid slot
/// started AFTER the claim was filed (PID reuse).
pub(crate) fn is_live(rec: &ClaimRecord) -> bool {
    liveness_reading(rec, &|pid| probe_pid(pid)).0
}

pub(crate) fn is_expired(rec: &ClaimRecord, now: i64) -> bool {
    match rec.expires_at {
        Some(exp) => now >= exp,
        None => false,
    }
}

/// Compose liveness + expiry into a state (mirrors `staleness.classify`,
/// INCLUDING the corroborated hybrid arm: an expired-TTL claim whose recorded
/// pid is a live process on this host is still LIVE only when that pid was
/// prover-proven at write time AND the record's harness forks per session - a
/// suspended-but-alive session must not have its claim reclaimed by a peer,
/// while a live FOREIGN pid (a chat app's app-server answering for the holder)
/// must not make the lease permanent).
///
/// SUSPECT arm: a TTL claim still inside its window whose recorded pid
/// is NOT a live process reads `Suspect`, not `Live`. Dead-pid-but-unexpired is
/// the respawned-worker case (supervisor pid died, session lives on): the TTL
/// keeps protecting the slot, so acquire/dispatch treat it like `Live` (never
/// steal), but the distinct state lets init/dispatch branch on it. Only TTL
/// expiry frees the claim (-> `Stale`); pid death alone never does.
///
/// SUSPECT also covers the unreadable holder on a pid-liveness claim:
/// a probe refusal means the process EXISTS and refuses inspection, so it is
/// not a proof of death and must never free the claim on pid evidence.
pub fn classify(rec: &ClaimRecord, now: Option<i64>) -> ClaimState {
    classify_with_basis(rec, now, &|pid| probe_pid(pid)).0
}

pub(crate) fn classify_with_session_witness(
    rec: &ClaimRecord,
    session_witness: Option<SessionWitness<'_>>,
) -> ClaimState {
    let now = now_ms();
    let probe = |pid| probe_pid(pid);
    classify_with_basis_and_exclusivity(rec, Some(now), &probe, None, session_witness).0
}

/// `classify` beside its basis, with the pid probe injectable (mirrors
/// `staleness.classify_with_basis`; the parity harness pins the vocabulary).
/// The basis names WHY, one cause per way a verdict can arise: `live`,
/// `ttl-expired`, or the liveness cause that failed (`offhost`,
/// `pid-unavailable`, `pid-absent`, `access-denied`, `pid-reuse`).
pub fn classify_with_basis(
    rec: &ClaimRecord,
    now: Option<i64>,
    probe: &dyn Fn(i32) -> PidProbe,
) -> (ClaimState, &'static str) {
    classify_with_basis_and_exclusivity(rec, now, probe, None, None)
}

/// The pid verdict for a claim whose holder is one short-lived process.
/// `gate:` keys read it at any age: the gate pid holds the mutex for the
/// whole hold, so a dead pid means no holder. Other short-lived holders read
/// it only at TTL expiry. Live keeps the claim; any pid cause except a
/// refused probe frees it (Stale, reapable); a refusal falls through - a
/// refusal is not proof of death. None = no verdict; off-host records,
/// pid-less records, and refused probes keep the path they took before.
fn pid_verdict(
    rec: &ClaimRecord,
    probe: &dyn Fn(i32) -> PidProbe,
) -> Option<(ClaimState, &'static str)> {
    if !is_same_machine(&rec.host, rec.machine_id.as_deref())
        || rec.pid_unavailable
        || rec.pid.is_none()
    {
        return None;
    }
    let (live, cause) = liveness_reading(rec, probe);
    if live {
        return Some((ClaimState::Live, cause));
    }
    if cause != basis::ACCESS_DENIED {
        return Some((ClaimState::Stale, cause));
    }
    None
}

/// A pid the prover proved is the holder session's own process, on a harness
/// where that process dies with the session. Both TTL arms trust it.
fn proven_session_pid(rec: &ClaimRecord) -> bool {
    rec.pid_provenance.as_deref() == Some("session-prover")
        && pid_dies_with_session(rec.harness.as_deref())
}

/// Classify with optional sweep-time sibling evidence. `None` is the honest
/// value for single-key reads; a full scan passes the PID exclusivity map's
/// result for the record being classified. `session_witness` is the
/// session-keyed liveness reader; `None` keeps the pid-only
/// verdicts legacy records were characterized under.
pub fn classify_with_basis_and_exclusivity(
    rec: &ClaimRecord,
    now: Option<i64>,
    probe: &dyn Fn(i32) -> PidProbe,
    pid_exclusive: Option<bool>,
    session_witness: Option<SessionWitness<'_>>,
) -> (ClaimState, &'static str) {
    let now = now.unwrap_or_else(now_ms);
    // A `dispatch:` pid can predate the worker's exec, so it waits for expiry.
    // A gate pid never does, so it decides before the TTL ends.
    if rec.key.starts_with("gate:") {
        if let Some(verdict) = pid_verdict(rec, probe) {
            return verdict;
        }
    }
    // A blueprint-session claim leases the PLANNING WINDOW, clock-only: a
    // planner shares its parent's pid, so the hybrid arm and the witness
    // would heal the claim for the parent's whole life after a TaskStop.
    if rec.holder.starts_with(BLUEPRINT_HOLDER_PREFIX)
        && now
            >= rec
                .expires_at
                .unwrap_or(rec.acquired_at.saturating_add(BLUEPRINT_LEASE_MS))
    {
        return (ClaimState::Stale, basis::TTL_EXPIRED);
    }
    if is_expired(rec, now) {
        // A review hold is a lease on the review; the holder's session answers another question.
        if rec.key.starts_with("review:branch:") {
            return (ClaimState::Stale, basis::TTL_EXPIRED);
        }
        // A lease whose holder is ONE SHORT-LIVED PROCESS reads its recorded
        // pid as the verdict: `dispatch:` reservations and any lease its
        // writer stamped `holder-process`. The session witness asks about the
        // session that wrote the record, which outlives the process and must
        // not heal its lease.
        if rec.key.starts_with("dispatch:")
            || rec.pid_provenance.as_deref() == Some("holder-process")
        {
            if let Some(verdict) = pid_verdict(rec, probe) {
                return verdict;
            }
        }
        // Corroborated hybrid: the pid keeps the claim Live only when it was
        // proven to be the holder session's own process. Any other provenance
        // (or a legacy record with no field) is Stale, as a pre-hybrid claim
        // was: the TTL is a lease.
        //
        // The liveness reading is only load-bearing here for a prover-proven
        // pid; every other expired claim is Stale on the clock alone, so the
        // probe (a syscall per claim) is skipped on that path.
        //
        // The harness gate is what makes this true of RECORDS rather than of
        // writers. A stamp is written by the process being judged, so a guard
        // whose only evidence is that field is one field away from lying again
        // - which is exactly how a record written under codex, where the stamp
        // meant "the app-server is up", once read Live 3h45m past its TTL. The
        // record's own `harness` is independent evidence, already on every
        // record, so a pre-fix claim on disk and one from an older binary in a
        // mixed-version fleet both get the correct verdict here.
        if proven_session_pid(rec) {
            let (live, cause) = liveness_reading(rec, probe);
            if live {
                if pid_exclusive == Some(false) {
                    return (ClaimState::Suspect, basis::PID_SHARED);
                }
                return (ClaimState::Live, cause);
            }
        }
        // Session witness. The recorded pid is a corpse after every
        // harness resume, so pid arithmetic alone collapses UNKNOWN into a
        // verdict - 1509 collapsed it into alive (nothing reapable, the reaper
        // starved) and before it, into dead (a session that wrote 18 seconds
        // earlier read provably dead). The witness is the third
        // state's exit: a LIVE session heals the verdict, and an UNRESOLVED
        // one reads Suspect only inside a bounded grace, then Stale -
        // reapable by policy, never held for a proof that never arrives
        // (the update:fno deadlock). A pid whose exclusivity demoted the
        // hybrid arm still yields to the witness: session-keyed evidence
        // outranks arithmetic on a number the resume already invalidated.
        if let Some(witness) = session_witness {
            // Records with NO session id never reach the witness: they keep
            // byte-for-byte today's verdict, which every pre-change claim and
            // the reaper counts the 1511 revert restored depend on.
            if rec.session_id.as_deref().is_some_and(|s| !s.is_empty()) {
                let session_state = witness(rec);
                if (rec.key.starts_with("node:") || rec.key.starts_with("task:"))
                    && is_same_machine(&rec.host, rec.machine_id.as_deref())
                    && matches!(&session_state, SessionLiveness::Absent)
                {
                    return (ClaimState::Stale, basis::SESSION_ABSENT);
                }
                if let SessionLiveness::Live(witness_basis) = session_state {
                    return (ClaimState::Live, witness_basis);
                }
                if now < rec.expires_at.unwrap_or(now) + UNRESOLVED_GRACE_MS {
                    return (ClaimState::Suspect, basis::TTL_EXPIRED_UNRESOLVED);
                }
                return (ClaimState::Stale, basis::TTL_EXPIRED_UNRESOLVED);
            }
        }
        return (ClaimState::Stale, basis::TTL_EXPIRED);
    }
    // A lease whose holder is ONE SHORT-LIVED PROCESS reads its recorded pid
    // at any age, not only at expiry: the flight gate and the post-merge
    // sync hold their lease exactly as long as the process lives, so a
    // provably dead pid frees it inside the TTL window instead of refusing
    // every retry until expiry. A refused probe is not proof of death and
    // falls through to the TTL-window arms below.
    if rec.pid_provenance.as_deref() == Some(HOLDER_PROCESS) {
        if let Some(verdict) = pid_verdict(rec, probe) {
            return verdict;
        }
    }
    let (live, cause) = liveness_reading(rec, probe);
    if rec.expires_at.is_none() {
        if live {
            return (ClaimState::Live, cause);
        }
        // Unreadable is not provably dead: never free a claim on pid evidence
        // we were refused.
        if cause == basis::ACCESS_DENIED {
            return (ClaimState::Suspect, cause);
        }
        return (ClaimState::Stale, cause);
    }
    // TTL claim, still inside its window: live pid => Live, dead/replaced pid
    // => Suspect (TTL-protected, not stealable) - unless the session witness
    // proves the holder: a resumed session's recorded pid is
    // permanently dead, so without this heal the claim sits Suspect until the
    // heartbeat lapses and the dead pid decides at expiry.
    let witnessed = session_witness.and_then(|witness| {
        rec.session_id
            .as_deref()
            .filter(|session| !session.is_empty())
            .map(|_| witness(rec))
    });
    // The session's own live process outranks an Absent witness, in the same
    // order as the expired arm. An ambient pid is only a neighbour, so it
    // proves nothing and the witness still decides.
    if live && proven_session_pid(rec) {
        if pid_exclusive == Some(false) {
            return (ClaimState::Suspect, basis::PID_SHARED);
        }
        return (ClaimState::Live, cause);
    }
    if (rec.key.starts_with("node:") || rec.key.starts_with("task:"))
        && is_same_machine(&rec.host, rec.machine_id.as_deref())
        && matches!(witnessed, Some(SessionLiveness::Absent))
    {
        return (ClaimState::Stale, basis::SESSION_ABSENT);
    }
    if live {
        (ClaimState::Live, cause)
    } else {
        match witnessed {
            Some(SessionLiveness::Live(witness_basis)) => (ClaimState::Live, witness_basis),
            Some(SessionLiveness::Absent | SessionLiveness::Unresolved) | None => {
                (ClaimState::Suspect, cause)
            }
        }
    }
}

/// Classify one claim for a garbage-collection sweep. The bool is true only
/// when the claim is provably dead from this host; otherwise the bucket names
/// the reason it remains protected or opaque.
pub fn classify_for_sweep(
    rec: &ClaimRecord,
    now: Option<i64>,
    probe: &dyn Fn(i32) -> PidProbe,
    pid_exclusive: Option<bool>,
    session_witness: Option<SessionWitness<'_>>,
) -> (bool, &'static str) {
    let now = now.unwrap_or_else(now_ms);
    let same_machine = is_same_machine(&rec.host, rec.machine_id.as_deref());
    let unidentifiable = rec.machine_id.is_none();
    if !same_machine && !(unidentifiable && is_expired(rec, now)) {
        return (false, basis::OFFHOST);
    }
    let (state, _) =
        classify_with_basis_and_exclusivity(rec, Some(now), probe, pid_exclusive, session_witness);
    if state == ClaimState::Stale {
        return (true, "");
    }
    (
        false,
        if state == ClaimState::Suspect {
            "suspect"
        } else {
            "live"
        },
    )
}

/// Return sweep-time PID exclusivity keyed by the machine identity and pid.
/// A false value means one prover-visible pid names more than one distinct
/// holder; a single-key caller must pass `None` to classification because it
/// has no sibling evidence from which to establish this property.
pub fn pid_exclusivity(records: &[ClaimRecord]) -> std::collections::BTreeMap<(String, i32), bool> {
    let mut holders: std::collections::BTreeMap<(String, i32), std::collections::BTreeSet<String>> =
        std::collections::BTreeMap::new();
    for rec in records {
        let Some(pid) = rec.pid else { continue };
        let identity = rec.machine_id.clone().unwrap_or_else(|| rec.host.clone());
        holders
            .entry((identity, pid))
            .or_default()
            .insert(rec.holder.clone());
    }
    holders
        .into_iter()
        .map(|(key, holders)| (key, holders.len() <= 1))
        .collect()
}
