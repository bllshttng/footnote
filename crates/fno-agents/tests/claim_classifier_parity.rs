//! parity-stage: characterization
//! parity-oracle: fno.claims.staleness
//!
//! Characterization harness for the Rust claim classifier. The corpus was
//! driven through both implementations before the Python leg was deleted, and
//! its state+basis map is now the frozen contract for the sole implementation.
//! It covers the four liveness branch conditions (pid unavailable, off-machine,
//! pid unreported by the OS, pid reused, and their refused-inspection split)
//! plus the expired-TTL, hybrid and suspect arms.
//!
//! The former exclusions are now covered by the port:
//!
//! - `classify_for_sweep` is owned by Rust and feeds the native sweep door.
//! - PID exclusivity is computed across the scanned set by the Rust client.
//!
//! Goldens live under `tests/golden/claim_classifier/corpus.out`. They were
//! captured from the proven-correct Python leg before deletion; normal runs
//! never invoke Python.

use fno_agents::claims::{
    basis, machine_id, now_ms, ClaimRecord, SessionWitness, UNRESOLVED_GRACE_MS,
};
use serde_json::{json, Value};
use std::process::Command;

use common::{assert_golden as assert_golden_common, Golden};

mod common;

/// A pid no OS can have assigned: above every platform's pid ceiling (macOS
/// caps at 99998, Linux pid_max at 2^22), so both legs read it as unreported.
const ABSENT_PID: i64 = 2_000_000_000;

/// Read THIS machine's identity from the native fact readers so same-machine
/// fixtures exercise the production machine-id arm without a second runtime.
fn local_identity() -> (String, String) {
    let out = Command::new("hostname")
        .output()
        .expect("run native hostname probe");
    assert!(out.status.success(), "hostname probe failed");
    (
        machine_id(),
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
    )
}

/// The probe behavior a case wants. `Real` lets both legs ask the OS (the
/// self-pid and impossible-pid cases read identically on both); the injected
/// kinds drive the arms a live OS cannot produce deterministically - the
/// Refused arm above all. Injection happens at the Rust probe seam.
#[derive(Clone, Copy)]
enum ProbeSpec {
    Real,
    Refused,
}

/// One fixture case in the frozen characterization corpus.
struct Case {
    label: &'static str,
    pid: Option<i64>,
    pid_unavailable: bool,
    /// Same-machine cases carry the live identity so the machine arm decides;
    /// foreign cases carry an id that matches nothing.
    machine_id: Option<String>,
    host: String,
    acquired_at: i64,
    expires_at: Option<i64>,
    pid_provenance: Option<&'static str>,
    /// The record's own harness. Load-bearing on the expired arm: a
    /// shared-host harness never lets a live pid extend an expired lease, so a
    /// corpus that leaves this `None` everywhere cannot see the two legs
    /// disagree about codex.
    harness: Option<&'static str>,
    probe: ProbeSpec,
    /// The record's session id (x-a613). `None` keeps every pre-existing case
    /// on the pid-only verdicts its golden line froze; a session id routes
    /// the verdict through the injected session witness.
    session_id: Option<&'static str>,
    /// The session witness answer the case wants, injected at the witness
    /// seam the production sweep threads.
    witness: WitnessSpec,
}

/// The session witness behavior a session-arm case wants.
#[derive(Clone, Copy)]
enum WitnessSpec {
    /// Never answered (only valid with session_id: None).
    None,
    RegistryLive,
    TranscriptLive,
    Unresolved,
}

fn corpus(now: i64, self_pid: i64, identity: &(String, String)) -> Vec<Case> {
    let past = now - 60_000;
    let future = now + 3_600_000;
    let (machine, hostname) = identity;
    // With a readable machine id, same-machine fixtures compare machine ids
    // (the production path). Without one, both legs fall back to the hostname
    // compare, which reaches the same decision on the same host.
    let local_machine: Option<String> = if machine.is_empty() {
        None
    } else {
        Some(machine.clone())
    };
    let local_host = if machine.is_empty() {
        hostname.clone()
    } else {
        "this-host.invalid".to_string()
    };

    vec![
        Case {
            label: "live_pid_liveness",
            pid: Some(self_pid),
            pid_unavailable: false,
            machine_id: local_machine.clone(),
            host: local_host.clone(),
            acquired_at: now,
            expires_at: None,
            pid_provenance: None,
            harness: None,
            probe: ProbeSpec::Real,
            session_id: None,
            witness: WitnessSpec::None,
        },
        Case {
            label: "live_ttl_unexpired",
            pid: Some(self_pid),
            pid_unavailable: false,
            machine_id: local_machine.clone(),
            host: local_host.clone(),
            acquired_at: now,
            expires_at: Some(future),
            pid_provenance: None,
            harness: None,
            probe: ProbeSpec::Real,
            session_id: None,
            witness: WitnessSpec::None,
        },
        Case {
            label: "pid_absent_pid_liveness",
            pid: Some(ABSENT_PID),
            pid_unavailable: false,
            machine_id: local_machine.clone(),
            host: local_host.clone(),
            acquired_at: now,
            expires_at: None,
            pid_provenance: None,
            harness: None,
            probe: ProbeSpec::Real,
            session_id: None,
            witness: WitnessSpec::None,
        },
        Case {
            label: "pid_reused_pid_liveness",
            // The live harness pid with acquired_at=0: whatever create time
            // the OS reports is AFTER the claim was filed, which is the
            // reuse condition, deterministically.
            pid: Some(self_pid),
            pid_unavailable: false,
            machine_id: local_machine.clone(),
            host: local_host.clone(),
            acquired_at: 0,
            expires_at: None,
            pid_provenance: None,
            harness: None,
            probe: ProbeSpec::Real,
            session_id: None,
            witness: WitnessSpec::None,
        },
        Case {
            label: "offhost_pid_liveness",
            pid: Some(self_pid),
            pid_unavailable: false,
            machine_id: Some("parity-not-this-machine-0000".to_string()),
            host: "somewhere-else.invalid".to_string(),
            acquired_at: now,
            expires_at: None,
            pid_provenance: None,
            harness: None,
            probe: ProbeSpec::Real,
            session_id: None,
            witness: WitnessSpec::None,
        },
        Case {
            label: "offhost_ttl_unexpired",
            pid: Some(self_pid),
            pid_unavailable: false,
            machine_id: Some("parity-not-this-machine-0000".to_string()),
            host: "somewhere-else.invalid".to_string(),
            acquired_at: now,
            expires_at: Some(future),
            pid_provenance: None,
            harness: None,
            probe: ProbeSpec::Real,
            session_id: None,
            witness: WitnessSpec::None,
        },
        Case {
            label: "pid_unavailable_ttl_unexpired",
            pid: None,
            pid_unavailable: true,
            machine_id: local_machine.clone(),
            host: local_host.clone(),
            acquired_at: now,
            expires_at: Some(future),
            pid_provenance: None,
            harness: None,
            probe: ProbeSpec::Real,
            session_id: None,
            witness: WitnessSpec::None,
        },
        Case {
            label: "suspect_ttl_unexpired_dead_pid",
            pid: Some(ABSENT_PID),
            pid_unavailable: false,
            machine_id: local_machine.clone(),
            host: local_host.clone(),
            acquired_at: now,
            expires_at: Some(future),
            pid_provenance: None,
            harness: None,
            probe: ProbeSpec::Real,
            session_id: None,
            witness: WitnessSpec::None,
        },
        Case {
            label: "stale_ttl_expired_unproven_live_pid",
            // A live pid under any other provenance loses at expiry: the TTL
            // is a lease, and an unproven pid cannot corroborate it.
            pid: Some(self_pid),
            pid_unavailable: false,
            machine_id: local_machine.clone(),
            host: local_host.clone(),
            acquired_at: now,
            expires_at: Some(past),
            pid_provenance: None,
            harness: None,
            probe: ProbeSpec::Real,
            session_id: None,
            witness: WitnessSpec::None,
        },
        Case {
            label: "hybrid_live_expired_proven_live_pid",
            pid: Some(self_pid),
            pid_unavailable: false,
            machine_id: local_machine.clone(),
            host: local_host.clone(),
            acquired_at: now,
            expires_at: Some(past),
            pid_provenance: Some("session-prover"),
            harness: None,
            probe: ProbeSpec::Real,
            session_id: None,
            witness: WitnessSpec::None,
        },
        Case {
            // The specimen, in fixture form. Identical to the case above in
            // every field but `harness`: an expired lease, prover-stamped, its
            // pid answering. Under claude that reads Live and must keep doing
            // so. Under codex the answering pid is a shared app-server that
            // outlives every session it hosts, so the reading is worthless and
            // the clock decides alone.
            label: "shared_host_expired_proven_live_pid_stales",
            pid: Some(self_pid),
            pid_unavailable: false,
            machine_id: local_machine.clone(),
            host: local_host.clone(),
            acquired_at: now,
            expires_at: Some(past),
            pid_provenance: Some("session-prover"),
            harness: Some("codex"),
            probe: ProbeSpec::Real,
            session_id: None,
            witness: WitnessSpec::None,
        },
        Case {
            // The must-not-break twin, named so a fix that stales everything
            // fails here rather than in production.
            label: "per_session_host_expired_proven_live_pid_survives",
            pid: Some(self_pid),
            pid_unavailable: false,
            machine_id: local_machine.clone(),
            host: local_host.clone(),
            acquired_at: now,
            expires_at: Some(past),
            pid_provenance: Some("session-prover"),
            harness: Some("claude"),
            probe: ProbeSpec::Real,
            session_id: None,
            witness: WitnessSpec::None,
        },
        Case {
            label: "stale_ttl_expired_proven_dead_pid",
            pid: Some(ABSENT_PID),
            pid_unavailable: false,
            machine_id: local_machine.clone(),
            host: local_host.clone(),
            acquired_at: now,
            expires_at: Some(past),
            pid_provenance: Some("session-prover"),
            harness: None,
            probe: ProbeSpec::Real,
            session_id: None,
            witness: WitnessSpec::None,
        },
        Case {
            // The holder EXISTS and refuses inspection: never a proof of
            // death, so this is SUSPECT where the gone-pid case above is
            // STALE - the split the probe fix delivered.
            label: "access_denied_pid_liveness",
            pid: Some(self_pid),
            pid_unavailable: false,
            machine_id: local_machine.clone(),
            host: local_host.clone(),
            acquired_at: now,
            expires_at: None,
            pid_provenance: None,
            harness: None,
            probe: ProbeSpec::Refused,
            session_id: None,
            witness: WitnessSpec::None,
        },
        Case {
            label: "access_denied_ttl_unexpired",
            pid: Some(self_pid),
            pid_unavailable: false,
            machine_id: local_machine.clone(),
            host: local_host.clone(),
            acquired_at: now,
            expires_at: Some(future),
            pid_provenance: None,
            harness: None,
            probe: ProbeSpec::Refused,
            session_id: None,
            witness: WitnessSpec::None,
        },
        Case {
            // x-0c29, in fixture form: the TTL expired and the recorded pid is
            // gone, but the session id's registry row carries a live pid. The
            // witness heals the verdict pid arithmetic read as provably dead.
            label: "resumed_session_expired_dead_pid_registry_live",
            pid: Some(ABSENT_PID),
            pid_unavailable: false,
            machine_id: local_machine.clone(),
            host: local_host.clone(),
            acquired_at: now,
            expires_at: Some(past),
            pid_provenance: Some("session-prover"),
            harness: None,
            probe: ProbeSpec::Real,
            session_id: Some("ses_parity"),
            witness: WitnessSpec::RegistryLive,
        },
        Case {
            // x-ba96, in fixture form: the lease is still inside its window
            // and the recorded pid is permanently dead (the resume shape). A
            // live transcript witness ends the Suspect limbo.
            label: "resumed_session_in_window_dead_pid_transcript_live",
            pid: Some(ABSENT_PID),
            pid_unavailable: false,
            machine_id: local_machine.clone(),
            host: local_host.clone(),
            acquired_at: now,
            expires_at: Some(future),
            pid_provenance: Some("session-prover"),
            harness: None,
            probe: ProbeSpec::Real,
            session_id: Some("ses_parity"),
            witness: WitnessSpec::TranscriptLive,
        },
        Case {
            // The bounded unknown: expired, no witness answer either way,
            // inside the grace window. Protected, never stealable, never
            // provably dead - Suspect with a named basis.
            label: "session_expired_unresolved_within_grace",
            pid: Some(ABSENT_PID),
            pid_unavailable: false,
            machine_id: local_machine.clone(),
            host: local_host.clone(),
            acquired_at: now,
            expires_at: Some(past),
            pid_provenance: Some("session-prover"),
            harness: None,
            probe: ProbeSpec::Real,
            session_id: Some("ses_parity"),
            witness: WitnessSpec::Unresolved,
        },
        Case {
            // The same unknown PAST the grace: Stale, reapable by policy -
            // the arm PR 1509 destroyed when unknown collapsed into alive and
            // the reaper starved (assert 0 == 5). Exits on a clock, never on
            // a proof that never arrives.
            label: "session_expired_unresolved_past_grace",
            pid: Some(ABSENT_PID),
            pid_unavailable: false,
            machine_id: local_machine.clone(),
            host: local_host.clone(),
            acquired_at: now,
            expires_at: Some(past - UNRESOLVED_GRACE_MS - 1),
            pid_provenance: Some("session-prover"),
            harness: None,
            probe: ProbeSpec::Real,
            session_id: Some("ses_parity"),
            witness: WitnessSpec::Unresolved,
        },
        Case {
            // The must-not-change twin for the witness itself: no session id
            // on the record, so even a live-answering witness is never
            // consulted and the pre-change verdict stands byte for byte.
            label: "session_absent_expired_ignores_witness",
            pid: Some(ABSENT_PID),
            pid_unavailable: false,
            machine_id: local_machine.clone(),
            host: local_host.clone(),
            acquired_at: now,
            expires_at: Some(past),
            pid_provenance: Some("session-prover"),
            harness: None,
            probe: ProbeSpec::Real,
            session_id: None,
            witness: WitnessSpec::RegistryLive,
        },
    ]
}

fn record(c: &Case) -> ClaimRecord {
    ClaimRecord {
        schema_version: if c.pid_unavailable { 2 } else { 1 },
        key: "parity".to_string(),
        holder: "parity".to_string(),
        acquired_at: c.acquired_at,
        pid: c.pid.map(|p| p as i32),
        host: c.host.clone(),
        pid_unavailable: c.pid_unavailable,
        expires_at: c.expires_at,
        reason: None,
        harness: c.harness.map(str::to_string),
        session_id: c.session_id.map(str::to_string),
        pid_provenance: c.pid_provenance.map(str::to_string),
        machine_id: c.machine_id.clone(),
        metadata: Default::default(),
    }
}

/// Classify the corpus with Rust and return the same JSON shape as the frozen
/// oracle. Keeping the whole corpus in one golden includes every case's state
/// and basis in the characterization contract.
fn rust_classify_all(cases: &[Case], now: i64) -> Value {
    let mut rows = serde_json::Map::new();
    for c in cases {
        let probe: &dyn Fn(i32) -> fno_agents::claims::PidProbe = match c.probe {
            ProbeSpec::Real => &|pid| fno_agents::claims::probe_pid(pid),
            ProbeSpec::Refused => &|_| fno_agents::claims::PidProbe::Refused,
        };
        let witness: SessionWitness = match c.witness {
            WitnessSpec::None => &|_| fno_agents::claims::SessionLiveness::Unresolved,
            WitnessSpec::RegistryLive => {
                &|_| fno_agents::claims::SessionLiveness::Live(basis::REGISTRY_SESSION_LIVE)
            }
            WitnessSpec::TranscriptLive => {
                &|_| fno_agents::claims::SessionLiveness::Live(basis::TRANSCRIPT_LIVE)
            }
            WitnessSpec::Unresolved => &|_| fno_agents::claims::SessionLiveness::Unresolved,
        };
        let (state, basis) = fno_agents::claims::classify_with_basis_and_exclusivity(
            &record(c),
            Some(now),
            probe,
            None,
            Some(witness),
        );
        rows.insert(
            c.label.to_string(),
            json!({"state": state.as_str(), "basis": basis}),
        );
    }
    Value::Object(rows)
}

#[test]
fn classify_state_and_basis_matches_frozen_golden() {
    let now = now_ms();
    let self_pid = std::process::id() as i64;
    let cases = corpus(now, self_pid, &local_identity());
    let rust = rust_classify_all(&cases, now);
    let rust_golden = Golden {
        exit: None,
        streams: vec![serde_json::to_string(&rust).unwrap()],
    };
    assert_golden_common("claim_classifier", "corpus", &rust_golden, None);
    // A zero-case pass is an absence, not a verdict: the corpus must actually
    // carry every state and basis into the frozen contract.
    assert!(
        cases.len() >= 10,
        "corpus contains only {} cases",
        cases.len()
    );
    eprintln!(
        "claim classifier corpus: {} cases frozen or verified",
        cases.len()
    );
}
