use super::*;

fn dead_pid() -> u32 {
    let mut candidate = 999_999u32;
    while std::path::Path::new(&format!("/proc/{candidate}")).exists()
        || unsafe { libc::kill(candidate as i32, 0) } == 0
    {
        candidate += 1;
    }
    candidate
}

fn session_record(pid: i32, acquired_at: i64, expires_at: Option<i64>) -> ClaimRecord {
    ClaimRecord {
        schema_version: SCHEMA_VERSION,
        key: "session:x".into(),
        holder: "h".into(),
        acquired_at,
        pid: Some(pid),
        host: hostname(),
        pid_unavailable: false,
        expires_at,
        reason: None,
        harness: None,
        session_id: Some("ses_witness".into()),
        pid_provenance: Some("ambient".into()),
        machine_id: None,
        metadata: serde_json::Map::new(),
    }
}

#[test]
fn claim_session_absent_releases_an_unexpired_node_claim_with_a_live_ambient_pid() {
    let now = now_ms();
    let mut rec = session_record(std::process::id() as i32, now, Some(now + 3_600_000));
    rec.key = "node:x-absent".into();
    let witness: SessionWitness = &|_| SessionLiveness::Absent;
    assert_eq!(
        classify_with_basis_and_exclusivity(&rec, Some(now), &probe_pid, None, Some(witness)),
        (ClaimState::Stale, basis::SESSION_ABSENT)
    );
}

#[test]
fn claim_session_absent_does_not_turn_unresolved_into_early_release() {
    let now = now_ms();
    let mut rec = session_record(dead_pid() as i32, now, Some(now + 3_600_000));
    rec.key = "node:x-unresolved".into();
    let witness: SessionWitness = &|_| SessionLiveness::Unresolved;
    assert_eq!(
        classify_with_basis_and_exclusivity(&rec, Some(now), &probe_pid, None, Some(witness)),
        (ClaimState::Suspect, basis::PID_ABSENT)
    );
}

#[test]
fn claim_session_absent_keeps_an_unexpired_offhost_claim_protected() {
    let now = now_ms();
    let mut rec = session_record(dead_pid() as i32, now, Some(now + 3_600_000));
    rec.key = "node:x-offhost".into();
    rec.host = "remote-host".into();
    rec.machine_id = Some("remote-machine".into());
    let witness: SessionWitness = &|_| SessionLiveness::Absent;
    assert_eq!(
        classify_with_basis_and_exclusivity(&rec, Some(now), &probe_pid, None, Some(witness)),
        (ClaimState::Suspect, basis::OFFHOST)
    );
}

#[test]
fn claim_session_absent_skips_unresolved_grace_after_expiry() {
    let now = now_ms();
    let mut rec = session_record(dead_pid() as i32, now - 1, Some(now - 1));
    rec.key = "node:x-expired-absent".into();
    let witness: SessionWitness = &|_| SessionLiveness::Absent;
    assert_eq!(
        classify_with_basis_and_exclusivity(&rec, Some(now), &probe_pid, None, Some(witness)),
        (ClaimState::Stale, basis::SESSION_ABSENT)
    );
}

fn session_prover_node_record(pid: i32, now: i64) -> ClaimRecord {
    let mut rec = session_record(pid, now, Some(now + 7_200_000));
    rec.key = "node:x-live".into();
    rec.harness = Some("claude".into());
    rec.pid_provenance = Some("session-prover".into());
    rec
}

#[test]
fn claim_session_absent_keeps_an_unexpired_node_claim_with_a_live_session_prover_pid() {
    let now = now_ms();
    let rec = session_prover_node_record(std::process::id() as i32, now);
    let witness: SessionWitness = &|_| SessionLiveness::Absent;
    let (state, _) =
        classify_with_basis_and_exclusivity(&rec, Some(now), &probe_pid, None, Some(witness));
    assert_eq!(state, ClaimState::Live);
}

#[test]
fn claim_session_absent_demotes_a_shared_session_prover_pid_to_suspect() {
    let now = now_ms();
    let rec = session_prover_node_record(std::process::id() as i32, now);
    let witness: SessionWitness = &|_| SessionLiveness::Absent;
    assert_eq!(
        classify_with_basis_and_exclusivity(
            &rec,
            Some(now),
            &probe_pid,
            Some(false),
            Some(witness)
        ),
        (ClaimState::Suspect, basis::PID_SHARED)
    );
}

#[test]
fn claim_session_absent_releases_an_unexpired_node_claim_with_a_dead_session_prover_pid() {
    let now = now_ms();
    let rec = session_prover_node_record(dead_pid() as i32, now);
    let witness: SessionWitness = &|_| SessionLiveness::Absent;
    assert_eq!(
        classify_with_basis_and_exclusivity(&rec, Some(now), &probe_pid, None, Some(witness)),
        (ClaimState::Stale, basis::SESSION_ABSENT)
    );
}
