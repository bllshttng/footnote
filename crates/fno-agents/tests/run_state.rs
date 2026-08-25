use fno_agents::run_state::{
    append_transition, fold_run_state, step, RunEvent, RunState, RunStateError,
};

#[test]
fn every_legal_transition_matches_the_plan_table() {
    let arms = [
        (
            RunState::Open,
            RunEvent::DispatchClassified,
            RunState::Working,
        ),
        (
            RunState::Working,
            RunEvent::DispatchClassified,
            RunState::Working,
        ),
        (
            RunState::Working,
            RunEvent::PrepareHandoff,
            RunState::Delegating,
        ),
        (
            RunState::Delegating,
            RunEvent::SuccessorProven,
            RunState::Closed,
        ),
        (
            RunState::Delegating,
            RunEvent::SuccessorUnproven,
            RunState::Working,
        ),
        (
            RunState::Working,
            RunEvent::TerminalDecided,
            RunState::Sealing,
        ),
        (RunState::Sealing, RunEvent::FinalizeDone, RunState::Closed),
    ];

    for (from, event, to) in arms {
        assert_eq!(step(from, event), Ok(to));
    }
    for from in [
        RunState::Open,
        RunState::Working,
        RunState::Delegating,
        RunState::Sealing,
    ] {
        assert_eq!(step(from, RunEvent::Cancel), Ok(RunState::Aborted));
        assert_eq!(step(from, RunEvent::Abort), Ok(RunState::Aborted));
    }
}

#[test]
fn named_invalid_transitions_are_errors() {
    let invalid = [
        (RunState::Delegating, RunEvent::PrepareHandoff),
        (RunState::Closed, RunEvent::DispatchClassified),
        (RunState::Working, RunEvent::FinalizeDone),
        (RunState::Sealing, RunEvent::DispatchClassified),
    ];
    for (from, event) in invalid {
        let error = step(from, event).unwrap_err();
        assert_eq!(error.from, from);
        assert_eq!(error.event, event);
    }
    for from in [
        RunState::Open,
        RunState::Working,
        RunState::Delegating,
        RunState::Sealing,
        RunState::Closed,
        RunState::Aborted,
    ] {
        assert!(step(from, RunEvent::ReleaseClaim).is_err());
    }
}

#[test]
fn corrupt_run_log_is_an_error_never_open() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("run-log.jsonl");
    std::fs::write(&log, "{not-json}\n").unwrap();
    assert!(matches!(
        fold_run_state(&log, "20260823T060900Z-cx73523-e04109"),
        Err(RunStateError::MalformedLine { line: 1, .. })
    ));
}

#[test]
fn append_and_fold_are_keyed_on_the_full_run_id() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("run-log.jsonl");
    let run_a = "20260823T060900Z-cx73523-e04109";
    let run_b = "20260823T060900Z-cx73523-deadbe";

    append_transition(&log, run_a, RunEvent::DispatchClassified).unwrap();
    append_transition(&log, run_b, RunEvent::DispatchClassified).unwrap();
    append_transition(&log, run_a, RunEvent::TerminalDecided).unwrap();

    assert_eq!(fold_run_state(&log, run_a).unwrap(), RunState::Sealing);
    assert_eq!(fold_run_state(&log, run_b).unwrap(), RunState::Working);
}

#[test]
fn rejected_transition_appends_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("run-log.jsonl");
    let run = "20260823T060900Z-cx73523-e04109";

    append_transition(&log, run, RunEvent::DispatchClassified).unwrap();
    append_transition(&log, run, RunEvent::PrepareHandoff).unwrap();
    append_transition(&log, run, RunEvent::SuccessorProven).unwrap();
    let before = std::fs::read_to_string(&log).unwrap();

    assert!(matches!(
        append_transition(&log, run, RunEvent::DispatchClassified),
        Err(RunStateError::InvalidTransition(_))
    ));
    assert_eq!(std::fs::read_to_string(&log).unwrap(), before);
}

#[test]
fn observer_lock_contention_returns_instead_of_waiting() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("run-log.jsonl");
    let holder = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(&log)
        .unwrap();
    holder.lock().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let thread_log = log.clone();
    let handle = std::thread::spawn(move || {
        tx.send(append_transition(
            &thread_log,
            "20260823T060900Z-cx73523-e04109",
            RunEvent::DispatchClassified,
        ))
        .unwrap();
    });

    let immediate = rx.recv_timeout(std::time::Duration::from_millis(200));
    std::fs::File::unlock(&holder).unwrap();
    handle.join().unwrap();

    assert!(matches!(immediate, Ok(Err(RunStateError::Io { .. }))));
}
