use super::*;

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

#[test]
fn bounded_run_completed_retains_status_stdout_and_capped_stderr_tail() {
    let tmp = tempfile::tempdir().unwrap();
    // Writes well past the retention cap to stderr, then JSON to stdout:
    // the drain must reach EOF anyway (or the child blocks on a full pipe
    // and the run degrades to a timeout), and the retained tail must be
    // exactly the cap.
    let fno = write_exec(
        tmp.path(),
        "fno",
        "#!/bin/sh\nhead -c 5000 /dev/zero | tr '\\0' 'x' 1>&2\necho '{\"ok\":true}'\n",
    );
    let cwd = std::env::temp_dir();
    match run_bounded(fno.as_os_str(), &[], &cwd, PROBE_TIMEOUT) {
        BoundedRun::Completed(out) => {
            assert!(out.status.success(), "status: {:?}", out.status);
            assert_eq!(out.stdout, b"{\"ok\":true}\n");
            assert!(
                out.stderr_tail.len() <= BOUNDED_STDERR_TAIL_CAP,
                "retained {} > cap {}",
                out.stderr_tail.len(),
                BOUNDED_STDERR_TAIL_CAP
            );
            // The tail, not the head: the retained bytes are the END of
            // the stream, which for a uniform fill is still all 'x' but
            // provably capped.
            assert_eq!(out.stderr_tail.len(), BOUNDED_STDERR_TAIL_CAP.min(9999));
            assert!(out.stderr_tail.iter().all(|&b| b == b'x'));
        }
        other => panic!("expected Completed, got {}", bounded_kind(&other)),
    }
}

#[test]
fn bounded_run_retains_the_last_stderr_bytes_not_the_first() {
    let tmp = tempfile::tempdir().unwrap();
    // A uniform fill followed by a marker at the END of the stream: the
    // retained bytes must carry the marker. A cap that freezes at the
    // first chunks (head-keeping) drops exactly this marker, and the
    // uniform-fill sibling above cannot tell head from tail - only this
    // shape distinguishes them.
    let fno = write_exec(
            tmp.path(),
            "fno",
            "#!/bin/sh\nhead -c 5000 /dev/zero | tr '\\0' 'x' 1>&2\necho TAIL_MARKER 1>&2\necho '{\"ok\":true}'\n",
        );
    let cwd = std::env::temp_dir();
    match run_bounded(fno.as_os_str(), &[], &cwd, PROBE_TIMEOUT) {
        BoundedRun::Completed(out) => {
            let tail = String::from_utf8_lossy(&out.stderr_tail);
            assert!(tail.ends_with("TAIL_MARKER\n"), "retained {tail:?}");
            assert_eq!(
                out.stderr_tail.len(),
                BOUNDED_STDERR_TAIL_CAP,
                "a stream past the cap retains exactly the cap"
            );
        }
        other => panic!("expected Completed, got {}", bounded_kind(&other)),
    }
}

#[test]
fn bounded_read_diagnostic_preserves_transport_classification() {
    let timeout = GhReadError::timed_out("main_run_view", std::time::Duration::from_secs(1));
    let rendered = bounded_read_diagnostic("main-head", &timeout);
    assert!(rendered.contains("main-head"), "{rendered}");
    assert!(rendered.contains("main_run_view"), "{rendered}");
    assert!(rendered.contains("outcome=timeout"), "{rendered}");
    assert!(rendered.contains("elapsed_s=1.0"), "{rendered}");

    let spawn = GhReadError::unrunnable("git_status", "spawn failed");
    let rendered = bounded_read_diagnostic("payload", &spawn);
    assert!(rendered.contains("git_status"), "{rendered}");
    assert!(rendered.contains("outcome=unrunnable"), "{rendered}");
    assert!(rendered.contains("spawn failed"), "{rendered}");
}

#[test]
fn bounded_run_completed_keeps_a_nonzero_exit_code_distinct_from_success() {
    let tmp = tempfile::tempdir().unwrap();
    let fno = write_exec(tmp.path(), "fno", "#!/bin/sh\necho boom 1>&2\nexit 3\n");
    let cwd = std::env::temp_dir();
    match run_bounded(fno.as_os_str(), &[], &cwd, PROBE_TIMEOUT) {
        BoundedRun::Completed(out) => {
            assert_eq!(out.status.code(), Some(3));
            assert!(!out.status.success());
            assert_eq!(out.stdout, b"");
            assert_eq!(out.stderr_tail, b"boom\n");
        }
        other => panic!("expected Completed, got {}", bounded_kind(&other)),
    }
}

#[test]
fn bounded_run_spawn_failure_reads_as_spawn_failed() {
    let cwd = std::env::temp_dir();
    let missing = Path::new("/definitely/missing/fno");
    assert!(matches!(
        run_bounded(missing.as_os_str(), &[], &cwd, PROBE_TIMEOUT),
        BoundedRun::SpawnFailed(std::io::ErrorKind::NotFound)
    ));
}

#[test]
fn bounded_run_kills_a_forked_process_group_on_timeout() {
    // A descendant keeps the stderr pipe open past the leader's death;
    // only a process-GROUP kill reaps it, and the run must resolve as a
    // distinct timeout inside the wall-clock budget, never as a hang.
    let tmp = tempfile::tempdir().unwrap();
    let fno = write_exec(
        tmp.path(),
        "fno",
        "#!/bin/sh\n(sleep 30 &) 1>&2\nsleep 30\n",
    );
    let cwd = std::env::temp_dir();
    let started = std::time::Instant::now();
    match run_bounded(
        fno.as_os_str(),
        &[],
        &cwd,
        std::time::Duration::from_millis(200),
    ) {
        BoundedRun::TimedOut(elapsed) => {
            assert!(elapsed >= std::time::Duration::from_millis(200));
            // Cleanup slack for spawn + group kill, generous for parallel
            // test scheduling: the point is "bounded", not "precise".
            assert!(started.elapsed() < std::time::Duration::from_secs(10));
        }
        other => panic!("expected TimedOut, got {}", bounded_kind(&other)),
    }
}

/// Render a `BoundedRun` variant name for assertion failures without a
/// Debug derive on the payload-carrying enum.
fn bounded_kind(run: &BoundedRun) -> &'static str {
    match run {
        BoundedRun::Completed(_) => "Completed",
        BoundedRun::TimedOut(_) => "TimedOut",
        BoundedRun::SpawnFailed(_) => "SpawnFailed",
        BoundedRun::WaitFailed => "WaitFailed",
    }
}

#[test]
fn no_direct_external_read_bypasses_bounded_runner() {
    // Production region only: test modules may legitimately spawn helper
    // processes. Every child module is production too, so a bypass hiding
    // in a named-by-question child cannot dodge a root-only scan.
    let production = production_source();
    // Positive control first, so an empty scan can never read as green:
    // the centralized runner must exist and carry real call sites.
    assert!(production.contains("fn run_bounded("));
    assert!(
        production.matches("bounded_read(").count() >= 10,
        "the bounded transport must carry its registered read sites"
    );
    assert!(
        production.matches("git_bounded(").count() >= 10,
        "the bounded transport must carry the stop-gate git read sites"
    );
    let bypasses = direct_wait_bypasses(&production);
    assert!(
        bypasses.is_empty(),
        "direct synchronous gh/fno/git waits outside the bounded runner: {bypasses:?}"
    );
}

#[test]
fn probe_reports_absent_only_for_a_missing_path() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("gh-not-there");
    assert_eq!(
        probe_gh_bin(missing.as_os_str(), tmp.path()),
        GhProbeOutcome::Absent
    );
}

#[test]
fn probe_reports_present_for_a_working_binary() {
    let tmp = tempfile::tempdir().unwrap();
    let gh = write_exec(tmp.path(), "gh", "#!/bin/sh\nexit 0\n");
    assert_eq!(
        probe_gh_bin(gh.as_os_str(), tmp.path()),
        GhProbeOutcome::Present
    );
}

/// ENOENT from an EXISTING script (its shebang interpreter is missing)
/// must read as spawn trouble, not absence: gh is present on disk, so
/// the session must not degrade to advisory mode over an interpreter
/// problem. The same ENOENT with no file at the path is real absence.
#[test]
fn probe_distinguishes_a_missing_interpreter_from_absence() {
    let tmp = tempfile::tempdir().unwrap();
    let gh = write_exec(tmp.path(), "gh", "#!/definitely/not/an/interp\nexit 0\n");
    assert_eq!(
        probe_gh_bin(gh.as_os_str(), tmp.path()),
        GhProbeOutcome::SpawnTrouble {
            kind: std::io::ErrorKind::NotFound
        }
    );
}

/// THE acceptance for the spawn-trouble class: an existing 644 file
/// fails to spawn with EACCES, a spawn error that is NOT absence, and
/// the probe must say spawn trouble - never "gh is absent".
#[test]
fn probe_reports_spawn_trouble_not_absent_for_a_non_executable_file() {
    let tmp = tempfile::tempdir().unwrap();
    let gh = tmp.path().join("gh-644");
    std::fs::write(&gh, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    let outcome = probe_gh_bin(gh.as_os_str(), tmp.path());
    assert_ne!(outcome, GhProbeOutcome::Absent);
    assert!(matches!(outcome, GhProbeOutcome::SpawnTrouble { .. }));
    assert_eq!(outcome.outcome_str(), "spawn_trouble");
}

/// ETXTBSY forced deterministically: a write fd held open across the
/// probe makes every exec attempt fail with ExecutableFileBusy, the
/// exact CI-load shape that used to read as "gh absent". Linux-only:
/// darwin's execve ignores a write fd held by another process (verified
/// 2026-08-29 - the exec succeeds), so on macOS the EACCES test above is
/// the portable stand-in for the non-NotFound spawn-error class.
#[cfg(target_os = "linux")]
#[test]
fn probe_reports_spawn_trouble_for_a_busy_text_file() {
    let tmp = tempfile::tempdir().unwrap();
    let gh = write_exec(tmp.path(), "gh", "#!/bin/sh\nexit 0\n");
    let _held = std::fs::OpenOptions::new().write(true).open(&gh).unwrap();
    let outcome = probe_gh_bin(gh.as_os_str(), tmp.path());
    assert_ne!(outcome, GhProbeOutcome::Absent);
    assert_eq!(
        outcome,
        GhProbeOutcome::SpawnTrouble {
            kind: std::io::ErrorKind::ExecutableFileBusy
        }
    );
}
