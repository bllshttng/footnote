use super::*;

// Hermetic: classify canned JSON without spawning, and exercise missing
// process handling separately. Mirrors the merge-gate half (tested in
// Python); the two readers are independent by design.

#[test]
fn plan_fidelity_gate_blocks_an_uncovered_shortfall() {
    match classify_plan_fidelity(br#"{"refused": true, "reason": "1 unjoined, 0 carveouts"}"#) {
        FidelityGate::Refused { reason } => assert!(reason.contains("unjoined")),
        other => panic!("expected Refused, got {:?}", other),
    }
}

#[test]
fn plan_fidelity_gate_passes_when_not_refused() {
    assert!(matches!(
        classify_plan_fidelity(br#"{"refused": false}"#),
        FidelityGate::Pass
    ));
}

#[test]
fn plan_fidelity_gate_absent_without_a_plan() {
    let cwd = std::env::temp_dir();
    let missing = Path::new("/definitely/missing/fno");
    assert!(matches!(
        evaluate_plan_fidelity(None, missing.as_os_str(), &cwd, FIDELITY_TIMEOUT),
        FidelityGate::Absent
    ));
    assert!(matches!(
        evaluate_plan_fidelity(Some(""), missing.as_os_str(), &cwd, FIDELITY_TIMEOUT),
        FidelityGate::Absent
    ));
}

#[test]
fn plan_fidelity_gate_fails_open_on_an_unparseable_or_missing_fno() {
    // A stale fno without the verb prints an error, not JSON. The stop gate
    // must not block on that - the merge gate is the backstop, and fail-open
    // here is what keeps a stale install from wedging every run.
    let cwd = std::env::temp_dir();
    assert!(matches!(
        evaluate_plan_fidelity(
            Some("/x/plan.md"),
            Path::new("/definitely/missing/fno").as_os_str(),
            &cwd,
            FIDELITY_TIMEOUT
        ),
        FidelityGate::Absent
    ));
    assert!(matches!(
        classify_plan_fidelity(b"No such command: fidelity"),
        FidelityGate::Absent
    ));
}

#[test]
fn plan_fidelity_gate_degrades_and_names_the_verb_on_timeout() {
    // a hung `fno do plan fidelity` child (was one concrete
    // cause; the bound must hold regardless of WHY the child hangs) must
    // be killed, not waited on forever, and must report as exactly that -
    // never a silent Absent pass, and never a misattributed message about
    // some unrelated read.
    let tmp = tempfile::tempdir().unwrap();
    let fno = write_exec(tmp.path(), "fno", "#!/bin/sh\nsleep 30\n");
    let cwd = std::env::temp_dir();
    let started = std::time::Instant::now();
    let gate = evaluate_plan_fidelity(
        Some("/x/plan.md"),
        fno.as_os_str(),
        &cwd,
        std::time::Duration::from_millis(200),
    );
    // The child is killed at the bound, not left to run out its sleep.
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    match gate {
        FidelityGate::Degraded { reason } => {
            assert!(reason.contains("fno do plan fidelity"), "{reason}");
            assert!(reason.contains("/x/plan.md"), "{reason}");
            assert!(reason.contains("timed out"), "{reason}");
            assert!(!reason.contains("gh read"), "{reason}");
            // Preserve fractional precision instead of truncating the
            // elapsed time to a flat "0.0s". Parallel test scheduling can
            // delay the observer beyond the nominal 200ms bound.
            assert!(reason.contains("timed out after "), "{reason}");
            assert!(!reason.contains("timed out after 0.0s"), "{reason}");
        }
        other => panic!("expected Degraded, got {other:?}"),
    }
}
