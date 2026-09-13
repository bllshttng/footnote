//! The `gate:` mutex liveness arm (x-dead direction three): a `gate:` record
//! names the short-lived process that held the machine-wide mutex, so the
//! recorded pid IS the verdict and a live spawning session never heals a dead
//! gate claim. Split from claims.rs to keep the over-budget file shrinking.

use super::*;

fn gate_record(pid: i32, acquired_at: i64, expires_at: Option<i64>) -> ClaimRecord {
    // The measured shape (x-dead direction three): holder
    // `spawn-gate:<pid>:<session>` on the global claims root, the pid of
    // the short-lived gate holder, the session id of the long-lived
    // SPAWNING session whose transcript witness used to heal it.
    ClaimRecord {
        key: "gate:spawn".into(),
        holder: format!("spawn-gate:{pid}:t-eacb-operator-pin"),
        session_id: Some("t-eacb-operator-pin".into()),
        ..ClaimRecord {
            schema_version: 1,
            key: String::new(),
            holder: String::new(),
            acquired_at,
            pid: Some(pid),
            host: hostname(),
            pid_unavailable: false,
            expires_at,
            reason: None,
            harness: None,
            session_id: None,
            pid_provenance: None,
            machine_id: None,
            metadata: Default::default(),
        }
    }
}

#[test]
fn an_expired_gate_claim_reads_the_pids_verdict() {
    // Dead holder pid, expired TTL: Stale by pid evidence, not by the
    // clock. Positive marker: the basis names the pid probe's cause.
    let now = now_ms();
    let (state, cause) =
        classify_with_basis(&gate_record(-1, now, Some(now - 1)), Some(now), &probe_pid);
    assert_eq!(state, ClaimState::Stale);
    assert_eq!(cause, basis::PID_ABSENT);
}

#[test]
fn a_live_gate_holder_pid_keeps_the_expired_claim_live() {
    let now = now_ms();
    let me = std::process::id() as i32;
    let (state, cause) =
        classify_with_basis(&gate_record(me, now, Some(now - 1)), Some(now), &probe_pid);
    assert_eq!(state, ClaimState::Live);
    assert_eq!(cause, basis::LIVE);
}

#[test]
fn a_live_spawning_session_never_heals_a_dead_gate_claim() {
    // THE specimen: the spawning session lives, the gate process does not.
    // The session witness answering Live must not reach the verdict.
    let now = now_ms();
    let witness: SessionWitness = &|_| SessionLiveness::Live(basis::TRANSCRIPT_LIVE);
    let (state, cause) = classify_with_basis_and_exclusivity(
        &gate_record(-1, now, Some(now - 1)),
        Some(now),
        &probe_pid,
        None,
        Some(witness),
    );
    assert_eq!(state, ClaimState::Stale);
    assert_eq!(cause, basis::PID_ABSENT);
}

#[test]
fn an_unexpired_gate_claim_with_a_dead_pid_stays_suspect() {
    // Only the expired arm changes; the unexpired arm keeps its
    // TTL-protected Suspect, matching the dispatch: precedent.
    let now = now_ms();
    let (state, _cause) = classify_with_basis(
        &gate_record(-1, now, Some(now + 60_000)),
        Some(now),
        &probe_pid,
    );
    assert_eq!(state, ClaimState::Suspect);
}

#[test]
fn an_offhost_gate_claim_skips_the_pid_arm() {
    // Off-host: this machine cannot probe the pid, so the verdict must
    // come from the clock path, not from a pid we never measured.
    let now = now_ms();
    let mut rec = gate_record(-1, now, Some(now - 1));
    rec.host = "elsewhere.example".into();
    let (state, cause) = classify_with_basis(&rec, Some(now), &probe_pid);
    assert_eq!(state, ClaimState::Stale);
    assert_eq!(cause, basis::TTL_EXPIRED);
}
