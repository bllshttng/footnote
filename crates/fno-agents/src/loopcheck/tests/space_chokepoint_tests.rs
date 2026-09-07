//! Decision-chokepoint tests that pin the spaces-move behavior: the run log
//! lands in the worktree slice of the repo's space, not in a checkout. A
//! child of `mod tests` so the private `allow_output` helper stays shared.

use super::{allow_output, observe_decision, TerminationReason};

/// Pins the state root inside `dir` and returns the resolved run log. The
/// returned guard holds the env lock for the test body, serializing against
/// the other FNO_AGENTS_HOME setters in this suite.
fn pin_space(dir: &std::path::Path) -> (std::sync::MutexGuard<'static, ()>, std::path::PathBuf) {
    let guard = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let agents_home = dir.join(".fnohome/agents");
    std::env::set_var("FNO_AGENTS_HOME", &agents_home);
    let run_log = crate::paths::worktree_space_dir(dir).join("run-log.jsonl");
    (guard, run_log)
}

#[test]
fn decision_chokepoint_observes_block_then_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let (_env, run_log) = pin_space(dir.path());
    let state = dir.path().join("target-state.md");
    let events = dir.path().join("events.jsonl");
    let run_id = "20260823T060900Z-cx73523-e04109";
    std::fs::write(&state, format!("---\nsession_id: {run_id}\n---\n")).unwrap();
    let args = vec![
        "loop-check".to_string(),
        "--state".to_string(),
        state.display().to_string(),
        "--transcript".to_string(),
        dir.path().join("transcript.jsonl").display().to_string(),
        "--cwd".to_string(),
        dir.path().display().to_string(),
        "--events".to_string(),
        events.display().to_string(),
        "--global-events".to_string(),
        events.display().to_string(),
    ];

    let blocked = allow_output("block", None, "keep working", 1, None);
    observe_decision(&args, &blocked);
    assert_eq!(
        crate::run_state::fold_run_state(&run_log, run_id).unwrap(),
        crate::run_state::RunState::Working
    );

    let terminal = allow_output(
        "allow",
        Some(TerminationReason::DonePRGreen),
        "done",
        2,
        None,
    );
    observe_decision(&args, &terminal);
    assert_eq!(
        crate::run_state::fold_run_state(&run_log, run_id).unwrap(),
        crate::run_state::RunState::Sealing
    );
    assert!(!events.exists());
}

#[test]
fn immediate_terminal_seeds_dispatch_before_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let (_env, run_log) = pin_space(dir.path());
    let state = dir.path().join("target-state.md");
    let events = dir.path().join("events.jsonl");
    let run_id = "20260823T060900Z-cx73523-e04109";
    std::fs::write(&state, format!("---\nfno_id: {run_id}\n---\n")).unwrap();
    let args = vec![
        "loop-check".to_string(),
        "--state".to_string(),
        state.display().to_string(),
        "--transcript".to_string(),
        dir.path().join("transcript.jsonl").display().to_string(),
        "--cwd".to_string(),
        dir.path().display().to_string(),
        "--events".to_string(),
        events.display().to_string(),
        "--global-events".to_string(),
        events.display().to_string(),
    ];

    observe_decision(
        &args,
        &allow_output(
            "allow",
            Some(TerminationReason::DonePRGreen),
            "done",
            1,
            None,
        ),
    );

    assert_eq!(
        crate::run_state::fold_run_state(&run_log, run_id).unwrap(),
        crate::run_state::RunState::Sealing
    );
    let log = std::fs::read_to_string(run_log).unwrap();
    assert!(log.contains("dispatch_classified"));
    assert!(log.contains("terminal_decided"));
    assert!(
        !events.exists(),
        "accepted shadow transitions emit no rejection"
    );
}

#[test]
fn decision_chokepoint_prefers_canonical_fno_id() {
    let dir = tempfile::tempdir().unwrap();
    let (_env, run_log) = pin_space(dir.path());
    let state = dir.path().join("target-state.md");
    let events = dir.path().join("events.jsonl");
    let canonical = "20260823T060900Z-cx73523-e04109";
    std::fs::write(
        &state,
        format!("---\nfno_id: {canonical}\nsession_id: legacy-run\n---\n"),
    )
    .unwrap();
    let args = vec![
        "loop-check".to_string(),
        "--state".to_string(),
        state.display().to_string(),
        "--transcript".to_string(),
        dir.path().join("transcript.jsonl").display().to_string(),
        "--cwd".to_string(),
        dir.path().display().to_string(),
        "--events".to_string(),
        events.display().to_string(),
        "--global-events".to_string(),
        events.display().to_string(),
    ];

    observe_decision(&args, &allow_output("block", None, "keep working", 1, None));

    assert_eq!(
        crate::run_state::fold_run_state(&run_log, canonical).unwrap(),
        crate::run_state::RunState::Working
    );
    assert_eq!(
        crate::run_state::fold_run_state(&run_log, "legacy-run").unwrap(),
        crate::run_state::RunState::Open
    );
}
