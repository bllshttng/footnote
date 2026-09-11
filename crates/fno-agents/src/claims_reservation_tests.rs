//! Key-class arm tests for the claim classifier, moved out of claims.rs when
//! the file crossed the 5,000-line shrink-only budget (x-41f7): the
//! classifier itself stays in claims.rs, these pins live beside it.

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

/// One `review:branch:`-shaped record: the hold a review dispatch takes,
/// carrying a session-prover pid and the reviewer session's own stamp - the
/// shape that let a TTL-lapsed hold read Live off its live session (x-b5f6).
fn review_hold(pid: i32, expires_at: Option<i64>, host: &str) -> ClaimRecord {
    ClaimRecord {
        schema_version: 1,
        key: "review:branch:feature/x".into(),
        holder: "review-session:s-reviewer".into(),
        acquired_at: now_ms(),
        pid: Some(pid),
        host: host.into(),
        pid_unavailable: false,
        expires_at,
        reason: None,
        harness: Some("claude".into()),
        session_id: Some("s-reviewer".into()),
        pid_provenance: Some("session-prover".into()),
        machine_id: None,
        metadata: serde_json::Map::new(),
    }
}

#[test]
fn an_expired_review_hold_lapses_while_its_holder_session_still_runs() {
    // The hold is a lease on the REVIEW: the transcript-live witness that kept
    // a lapsed hold bucket-live 7h past expiry (PR 1709 wedged) never reaches
    // this key class - the arm sits above both the hybrid and the witness.
    let me = std::process::id() as i32;
    let host = hostname();
    let now = now_ms();
    let witness: SessionWitness = &|_| SessionLiveness::Live(basis::TRANSCRIPT_LIVE);

    let lapsed = review_hold(me, Some(now - 1), &host);
    let (state, cause) =
        classify_with_basis_and_exclusivity(&lapsed, Some(now), &probe_pid, None, Some(witness));
    assert_eq!((state, cause), (ClaimState::Stale, basis::TTL_EXPIRED));
    let (provably_dead, _) =
        classify_for_sweep(&lapsed, Some(now), &probe_pid, None, Some(witness));
    assert!(provably_dead);

    // Scoping twin (x-37dd): the same expired shape under node: keeps today's
    // verdict - Live, whichever live evidence answers first (hybrid pid or
    // witness) - so no key class outside review:branch: flips polarity.
    let mut other_key = review_hold(me, Some(now - 1), &host);
    other_key.key = "node:x-t".into();
    let (state, _) =
        classify_with_basis_and_exclusivity(&other_key, Some(now), &probe_pid, None, Some(witness));
    assert_eq!(state, ClaimState::Live);
}

#[test]
fn an_unexpired_review_hold_with_a_dead_pid_still_protects() {
    // Inside the TTL the hold keeps its Suspect arm and review_activity keeps
    // blocking on it; only the expired arm changed.
    let host = hostname();
    let now = now_ms();
    let unexpired = review_hold(-1, Some(now + 60_000), &host);
    let (state, cause) = classify_with_basis(&unexpired, Some(now), &probe_pid);
    assert_eq!((state, cause), (ClaimState::Suspect, basis::PID_ABSENT));
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
