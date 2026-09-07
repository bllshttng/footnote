//! Reservation-arm tests, moved out of claims.rs when the file crossed the
//! 5,000-line shrink-only budget (x-41f7): the classifier itself stays in
//! claims.rs, these pins live beside it.

use super::*;

/// One `dispatch:`-shaped record: the holder names the dispatching pid and
/// the session stamp is the dispatcher's, the measured shape of every
/// boot-window reservation (ambient provenance, 10 of 10 dead pids).
fn reservation(pid: i32, expires_at: Option<i64>, host: &str) -> ClaimRecord {
    ClaimRecord {
        schema_version: 1,
        key: "dispatch:x-t".into(),
        holder: format!("spawn-cli:{pid}"),
        acquired_at: now_ms(),
        pid: Some(pid),
        host: host.into(),
        pid_unavailable: false,
        expires_at,
        reason: None,
        harness: None,
        session_id: Some("s-dispatcher".into()),
        pid_provenance: Some("ambient".into()),
        machine_id: None,
        metadata: serde_json::Map::new(),
    }
}

#[test]
fn expired_reservation_resolves_through_the_pid_it_names() {
    // AC4/AC5: the witness says the DISPATCHER's session is live - the
    // old reading kept the reservation bucket-live off exactly that. The
    // pid the holder names is the verdict instead: dead -> Stale and
    // provably dead, live -> Live and protected.
    let me = std::process::id() as i32;
    let host = hostname();
    let now = now_ms();
    let witness: SessionWitness = &|_| SessionLiveness::Live(basis::REGISTRY_SESSION_LIVE);

    let dead = reservation(-1, Some(now - 1), &host);
    let (state, cause) =
        classify_with_basis_and_exclusivity(&dead, Some(now), &probe_pid, None, Some(witness));
    assert_eq!((state, cause), (ClaimState::Stale, basis::PID_ABSENT));
    let (provably_dead, _) = classify_for_sweep(&dead, Some(now), &probe_pid, None, Some(witness));
    assert!(provably_dead);

    let live = reservation(me, Some(now - 1), &host);
    let (state, _) =
        classify_with_basis_and_exclusivity(&live, Some(now), &probe_pid, None, Some(witness));
    assert_eq!(state, ClaimState::Live);
    let (provably_dead, bucket) =
        classify_for_sweep(&live, Some(now), &probe_pid, None, Some(witness));
    assert!(!provably_dead);
    assert_eq!(bucket, "live");

    // Scoping: the arm is keyed on the dispatch: namespace. The same
    // expired shape under another key keeps answering from the session
    // witness, byte-for-byte.
    let mut other_key = reservation(-1, Some(now - 1), &host);
    other_key.key = "node:x-t".into();
    let (state, cause) =
        classify_with_basis_and_exclusivity(&other_key, Some(now), &probe_pid, None, Some(witness));
    assert_eq!(
        (state, cause),
        (ClaimState::Live, basis::REGISTRY_SESSION_LIVE)
    );
}

#[test]
fn unexpired_reservation_keeps_its_suspect_arm() {
    // AC6: inside the TTL a dead-pid reservation is TTL-protected - the
    // spawn CLI exits at launch while its worker boots. Only expiry
    // changes (the arm above).
    let host = hostname();
    let now = now_ms();
    let unexpired = reservation(-1, Some(now + 60_000), &host);
    let (state, cause) = classify_with_basis(&unexpired, Some(now), &probe_pid);
    assert_eq!((state, cause), (ClaimState::Suspect, basis::PID_ABSENT));
}

#[test]
fn offhost_or_pid_unavailable_reservation_keeps_todayss_verdict() {
    // AC7: no pid to ask - off-host, or recorded pid_unavailable - so the
    // arm stands down and the witness path answers as before. Marking
    // another machine's reservation stale would make it stealable.
    let host = hostname();
    let now = now_ms();
    let witness: SessionWitness = &|_| SessionLiveness::Live(basis::REGISTRY_SESSION_LIVE);

    let offhost = reservation(-1, Some(now - 1), "elsewhere.example");
    let (state, cause) =
        classify_with_basis_and_exclusivity(&offhost, Some(now), &probe_pid, None, Some(witness));
    assert_eq!(
        (state, cause),
        (ClaimState::Live, basis::REGISTRY_SESSION_LIVE)
    );

    let mut unp = reservation(-1, Some(now - 1), &host);
    unp.pid = None;
    unp.pid_unavailable = true;
    unp.schema_version = 2;
    let (state, cause) =
        classify_with_basis_and_exclusivity(&unp, Some(now), &probe_pid, None, Some(witness));
    assert_eq!(
        (state, cause),
        (ClaimState::Live, basis::REGISTRY_SESSION_LIVE)
    );
}

#[test]
fn pid_prober_control_alive_absent() {
    // AC8: the instrument itself, not a mock of it - the reservation
    // arm's verdicts ride on this prober. Own pid is alive, a
    // never-allocated pid is absent. (pid 1 measured NOT introspectable
    // on macOS - launchd refuses proc_pidinfo - so it is no control here.)
    assert!(matches!(
        probe_pid(std::process::id() as i32),
        PidProbe::Created(_)
    ));
    assert!(matches!(probe_pid(999_999), PidProbe::Absent));
}
