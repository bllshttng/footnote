//! The short-lived holder arm: `gate:` keys and `holder-process` leases name
//! the short-lived process that held them, so the recorded pid IS the verdict
//! and a live writing session never heals a dead process's claim. Gate keys
//! read it at any age; `holder-process` leases read it at expiry.
//! Split from claims.rs to keep the over-budget file shrinking.

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

fn lease_record(
    pid: i32,
    acquired_at: i64,
    expires_at: Option<i64>,
    provenance: &str,
) -> ClaimRecord {
    // The measured specimen: a post-merge sync lease written by a sync
    // subprocess that died, wearing the session stamp of the long-lived
    // session that ran the merge.
    ClaimRecord {
        key: "post-merge-sync".into(),
        holder: "sync-canonical:2033".into(),
        session_id: Some("s-crown".into()),
        pid_provenance: Some(provenance.into()),
        ..gate_record(pid, acquired_at, expires_at)
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
fn an_unexpired_gate_claim_with_a_dead_pid_reads_stale() {
    // The gate pid holds the mutex for the whole hold, so a dead pid frees
    // it at once instead of queueing every spawner behind the TTL.
    let now = now_ms();
    let (state, cause) = classify_with_basis(
        &gate_record(-1, now, Some(now + 60_000)),
        Some(now),
        &probe_pid,
    );
    assert_eq!(state, ClaimState::Stale);
    assert_eq!(cause, basis::PID_ABSENT);
}

#[test]
fn an_unexpired_gate_claim_with_a_live_pid_stays_live() {
    let now = now_ms();
    let me = std::process::id() as i32;
    let (state, cause) = classify_with_basis(
        &gate_record(me, now, Some(now + 60_000)),
        Some(now),
        &probe_pid,
    );
    assert_eq!(state, ClaimState::Live);
    assert_eq!(cause, basis::LIVE);
}

#[test]
fn an_unexpired_gate_claim_with_a_refused_probe_stays_suspect() {
    // A refusal is not proof of death.
    let now = now_ms();
    let (state, _cause) = classify_with_basis(
        &gate_record(-1, now, Some(now + 60_000)),
        Some(now),
        &|_| PidProbe::Refused,
    );
    assert_eq!(state, ClaimState::Suspect);
}

#[test]
fn an_unexpired_dispatch_claim_with_a_dead_pid_stays_suspect() {
    // A dispatch pid can predate the worker's exec, so only the gate arm
    // reads the pid inside the TTL.
    let now = now_ms();
    let rec = ClaimRecord {
        key: "dispatch:x-0000".into(),
        ..gate_record(-1, now, Some(now + 60_000))
    };
    let (state, _cause) = classify_with_basis(&rec, Some(now), &probe_pid);
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

#[test]
fn a_live_session_never_heals_an_expired_holder_process_lease() {
    // THE specimen: the sync process is dead, its TTL expired hours ago,
    // and the session that wrote the lease is still live. The pid is the
    // verdict; the witness must not heal it.
    let now = now_ms();
    let witness: SessionWitness = &|_| SessionLiveness::Live(basis::TRANSCRIPT_LIVE);
    let (state, cause) = classify_with_basis_and_exclusivity(
        &lease_record(-1, now, Some(now - 1), "holder-process"),
        Some(now),
        &probe_pid,
        None,
        Some(witness),
    );
    assert_eq!(state, ClaimState::Stale);
    assert_eq!(cause, basis::PID_ABSENT);
}

#[test]
fn a_live_holder_process_keeps_its_expired_lease_live() {
    let now = now_ms();
    let me = std::process::id() as i32;
    let (state, cause) = classify_with_basis(
        &lease_record(me, now, Some(now - 1), "holder-process"),
        Some(now),
        &probe_pid,
    );
    assert_eq!(state, ClaimState::Live);
    assert_eq!(cause, basis::LIVE);
}

#[test]
fn an_unstamped_expired_lease_still_heals_through_its_session() {
    // Legacy records keep today's verdict: an ambient stamp falls through
    // to the session witness, and a live witness heals the expired lease.
    let now = now_ms();
    let witness: SessionWitness = &|_| SessionLiveness::Live(basis::TRANSCRIPT_LIVE);
    let (state, cause) = classify_with_basis_and_exclusivity(
        &lease_record(-1, now, Some(now - 1), "ambient"),
        Some(now),
        &probe_pid,
        None,
        Some(witness),
    );
    assert_eq!(state, ClaimState::Live);
    assert_eq!(cause, basis::TRANSCRIPT_LIVE);
}

#[test]
fn an_unexpired_holder_process_lease_with_a_dead_pid_stays_suspect() {
    // Only the expired arm reads the pid; inside the TTL the lease stays
    // protected, matching the dispatch: precedent.
    let now = now_ms();
    let (state, _cause) = classify_with_basis(
        &lease_record(-1, now, Some(now + 60_000), "holder-process"),
        Some(now),
        &probe_pid,
    );
    assert_eq!(state, ClaimState::Suspect);
}

fn prov_opts(td: &tempfile::TempDir) -> AcquireOpts {
    AcquireOpts {
        root: Some(td.path().to_path_buf()),
        events_dir: Some(td.path().to_path_buf()),
        ..Default::default()
    }
}

#[test]
fn make_claim_stamps_pid_provenance_by_pid_origin() {
    let td = tempfile::TempDir::new().unwrap();
    // A defaulted pid is the claimant itself: the strongest provenance -
    // but only under a harness that forks per session. The suite's own
    // ambient harness decides which, so the expectation is derived rather
    // than hardcoded; hardcoding it makes this test pass under claude and
    // fail under codex, which is a flake keyed to who ran it.
    let own = match acquire("session:prov", "pty:me", prov_opts(&td)) {
        AcquireOutcome::Acquired(r) => r,
        other => panic!("{other:?}"),
    };
    let expected = if pid_dies_with_session(own.harness.as_deref()) {
        "session-prover"
    } else {
        "ambient"
    };
    assert_eq!(own.pid_provenance.as_deref(), Some(expected));
    // An explicitly passed pid is caller-supplied and unverifiable here:
    // ambient, so the hybrid arm will not extend an expired lease for it.
    let mut o = prov_opts(&td);
    o.pid = Some(4242);
    let foreign = match acquire("session:prov2", "pty:me", o) {
        AcquireOutcome::Acquired(r) => r,
        other => panic!("{other:?}"),
    };
    assert_eq!(foreign.pid_provenance.as_deref(), Some("ambient"));
}

#[test]
fn an_explicit_provenance_stamps_the_lease_verbatim() {
    // A writer that knows its pid holds the whole lease (the flight gate)
    // passes the stamp through: make_claim must not recompute it.
    let td = tempfile::TempDir::new().unwrap();
    let mut o = prov_opts(&td);
    o.pid = Some(4242);
    o.pid_provenance = Some(HOLDER_PROCESS.into());
    let rec = match acquire("session:prov3", "pty:me", o) {
        AcquireOutcome::Acquired(r) => r,
        other => panic!("{other:?}"),
    };
    assert_eq!(rec.pid_provenance.as_deref(), Some(HOLDER_PROCESS));
}
