//! parity-stage: differential
//! parity-oracle: fno.claims.staleness
//!
//! Differential harness for the claim classifier. Python's
//! `fno.claims.staleness.classify` and this crate's `claims::classify` each
//! carry a docstring claiming to mirror the other; this file is the instrument
//! that makes that claim checkable. One fixture corpus drives BOTH legs
//! through the four liveness branch conditions (pid unavailable, off-machine,
//! pid unreported by the OS, pid reused, and their refused-inspection split)
//! plus the expired-TTL, hybrid and suspect arms, and asserts identical
//! states AND identical bases for every case - the basis vocabulary cannot
//! drift the way the state once could.
//!
//! Excluded from the corpus, on purpose, one line each:
//!
//! - `classify_for_sweep`: Python-only, has no Rust counterpart, so it is not
//!   a dual implementation and there is nothing to pin it against.
//! - `pid_exclusive=False`: an input only the Python signature can express
//!   (Rust callers cannot pass it), so no reachable disagreement exists.
//!
//! Skips (does not fail) when `python3` is absent or the oracle is not
//! importable (its `psutil` dependency included), mirroring
//! `claude_ask_parity.rs`'s skip-when-unavailable policy. Set
//! `FNO_CLAIMS_PARITY_PYTHON` to point the harness at an interpreter that has
//! `psutil` installed (a bare `python3` often does not); CI installs it.

use fno_agents::claims::{now_ms, ClaimRecord};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Command;

use common::{assert_golden as assert_golden_common, capture_mode, Golden};

mod common;

/// A pid no OS can have assigned: above every platform's pid ceiling (macOS
/// caps at 99998, Linux pid_max at 2^22), so both legs read it as unreported.
const ABSENT_PID: i64 = 2_000_000_000;

/// Repo `cli/src` so Python can import the real `fno` package.
fn pythonpath() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../cli/src")
}

/// The interpreter that runs the oracle. An explicit `FNO_CLAIMS_PARITY_PYTHON`
/// wins; otherwise the repo's own `cli/.venv` is preferred over a bare
/// `python3`, because the oracle needs `psutil` and a bare `python3` usually
/// lacks it. Without that preference this harness SKIPS on a normal dev
/// machine and prints `ok` for a corpus it never compared - a green that means
/// "not verified here". It skipped through the very disagreement the
/// shared-host cases below now pin.
fn parity_python() -> String {
    if let Ok(explicit) = std::env::var("FNO_CLAIMS_PARITY_PYTHON") {
        return explicit;
    }
    let venv = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../cli/.venv/bin/python");
    if venv.is_file() {
        return venv.to_string_lossy().into_owned();
    }
    "python3".to_string()
}

fn python_available() -> bool {
    let probe = Command::new(parity_python())
        .arg("-c")
        .arg("import fno.claims.staleness")
        .env("PYTHONPATH", pythonpath())
        .output();
    matches!(probe, Ok(o) if o.status.success())
}

/// Read THIS machine's identity from the genuine Python oracle, so the
/// same-machine fixtures carry values both legs agree are local. Returns
/// (machine_id, hostname); machine_id is empty when the OS exposes none.
fn local_identity() -> (String, String) {
    let code = r#"
from fno.claims.hostid import machine_id
import socket
print(machine_id())
print(socket.gethostname())
"#;
    let out = Command::new(parity_python())
        .arg("-c")
        .arg(code)
        .env("PYTHONPATH", pythonpath())
        .output()
        .expect("run python hostid identity probe");
    assert!(
        out.status.success(),
        "python identity probe failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let machine = lines.next().unwrap_or("").trim().to_string();
    let hostname = lines.next().unwrap_or("").trim().to_string();
    (machine, hostname)
}

/// The probe behavior a case wants. `Real` lets both legs ask the OS (the
/// self-pid and impossible-pid cases read identically on both); the injected
/// kinds drive the arms a live OS cannot produce deterministically - the
/// Refused arm above all. Injection happens at each leg's own seam: the
/// `probe` parameter here, `_probe_create_time` monkeypatched there.
#[derive(Clone, Copy)]
enum ProbeSpec {
    Real,
    Absent,
    Refused,
}

/// One fixture case. Every field travels to both legs unchanged; the Python
/// subprocess rebuilds the equivalent `Claim` from the same JSON.
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
        },
    ]
}

fn to_json(c: &Case, now: i64) -> Value {
    let probe = match c.probe {
        ProbeSpec::Real => json!(null),
        ProbeSpec::Absent => json!({"kind": "absent"}),
        ProbeSpec::Refused => json!({"kind": "refused"}),
    };
    json!({
        "label": c.label,
        "schema_version": if c.pid_unavailable { 2 } else { 1 },
        "pid": c.pid,
        "pid_unavailable": c.pid_unavailable,
        "host": c.host,
        "machine_id": c.machine_id,
        "acquired_at": c.acquired_at,
        "expires_at": c.expires_at,
        "pid_provenance": c.pid_provenance,
        "harness": c.harness,
        "probe": probe,
        "now": now,
    })
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
        pid_provenance: c.pid_provenance.map(str::to_string),
        machine_id: c.machine_id.clone(),
        metadata: Default::default(),
    }
}

/// Run the genuine Python classifier over the corpus and return
/// `{label: {state, basis}}` for every case.
fn py_classify_all(cases_json: &str) -> Value {
    let code = r#"
import json, sys
from fno.claims import staleness
from fno.claims.staleness import classify_with_basis
from fno.claims.types import Claim

REAL_PROBE = staleness._probe_create_time
cases = json.load(sys.stdin)
rows = {}
for c in cases:
    spec = c.get("probe")
    if spec is not None:
        kind = spec["kind"]
        if kind == "absent":
            staleness._probe_create_time = lambda pid: (None, "pid-absent")
        elif kind == "refused":
            staleness._probe_create_time = lambda pid: (None, "access-denied")
        else:
            staleness._probe_create_time = lambda pid: (spec["ms"], "")
    else:
        staleness._probe_create_time = REAL_PROBE
    claim = Claim(
        schema_version=c["schema_version"],
        key="parity",
        holder="parity",
        acquired_at=c["acquired_at"],
        pid=c["pid"],
        pid_unavailable=c["pid_unavailable"],
        host=c["host"],
        machine_id=c["machine_id"],
        expires_at=c["expires_at"],
        pid_provenance=c["pid_provenance"],
        harness=c["harness"],
    )
    state, basis = classify_with_basis(claim, now=c["now"])
    rows[c["label"]] = {"state": state.value, "basis": basis}
json.dump(rows, sys.stdout)
"#;
    use std::io::Write;
    let mut child = Command::new(parity_python())
        .arg("-c")
        .arg(code)
        .env("PYTHONPATH", pythonpath())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn python classify");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(cases_json.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "python classify failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("python returned a JSON object")
}

/// Classify the corpus with the Rust leg and return the same JSON shape as the
/// Python oracle. Keeping the whole corpus in one golden makes the frozen
/// contract include every case's state and basis.
fn rust_classify_all(cases: &[Case], now: i64) -> Value {
    let mut rows = serde_json::Map::new();
    for c in cases {
        let probe: &dyn Fn(i32) -> fno_agents::claims::PidProbe = match c.probe {
            ProbeSpec::Real => &|pid| fno_agents::claims::probe_pid(pid),
            ProbeSpec::Absent => &|_| fno_agents::claims::PidProbe::Absent,
            ProbeSpec::Refused => &|_| fno_agents::claims::PidProbe::Refused,
        };
        let (state, basis) = fno_agents::claims::classify_with_basis(&record(c), Some(now), probe);
        rows.insert(
            c.label.to_string(),
            json!({"state": state.as_str(), "basis": basis}),
        );
    }
    Value::Object(rows)
}

#[test]
fn classify_state_and_basis_parity_with_real_python() {
    if capture_mode() && !python_available() {
        eprintln!(
            "SKIP: no python3 with fno.claims.staleness importable; parity not verified here"
        );
        return;
    }
    let now = now_ms();
    let self_pid = std::process::id() as i64;
    let cases = corpus(now, self_pid, &local_identity());
    let cases_json =
        serde_json::to_string(&cases.iter().map(|c| to_json(c, now)).collect::<Vec<_>>()).unwrap();
    let rust = rust_classify_all(&cases, now);
    let oracle = capture_mode().then(|| {
        let py = py_classify_all(&cases_json);
        Golden {
            exit: None,
            streams: vec![serde_json::to_string(&py).unwrap()],
        }
    });
    let rust_golden = Golden {
        exit: None,
        streams: vec![serde_json::to_string(&rust).unwrap()],
    };
    assert_golden_common("claim_classifier", "corpus", &rust_golden, oracle);
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
