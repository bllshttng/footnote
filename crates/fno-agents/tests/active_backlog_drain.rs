//! Integration tests for the mission drain tick (x-a4dc K2).
//!
//! These exercise `mission_drain_tick`'s real IO branches against a stub `fno`
//! binary (passed directly as `DrainConfig.abi_bin`) with the loop Journal
//! pointed at a tempdir so no real `~/.fno` state is touched. The mission drain
//! dispatches by shelling K1's converge core (`fno backlog advance --epic <id>
//! --json`) and reconciles the dispatched nodes from events on later ticks. The
//! reconcile POLICY (event -> breaker success/park, crash floor) needs seeded
//! `node:<id>` claims and lives in the src module's unit tests
//! (`FNO_CLAIMS_ROOT`-hermetic); this file covers the tick's IO wiring + the
//! `advance --epic` argv seam. Pending starts empty here, so the reconcile pass
//! at the top of the tick is a harmless no-op.
//!
//! Coverage map:
//!   dispatch  advance --epic returns ids -> pending recorded, Continue, event
//!   seam      the `advance --epic <mission> --json` argv is forwarded verbatim
//!   empty     advance dispatches nothing -> no pending, no dispatched event
//!   retire    advance reports all_done / deactivated -> Retire
//!   failure   advance exits non-zero -> Continue, skip journaled, no pending

use fno_agents::active_backlog::{
    mission_drain_tick, CircuitBreaker, DrainConfig, MissionDispatch,
};
use fno_agents::loop_runtime::Journal;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn write_stub(dir: &Path, name: &str, body: &str) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, format!("#!/usr/bin/env bash\n{body}\n").as_bytes()).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn journal_in(tmp: &Path) -> (Journal, PathBuf) {
    let project = tmp.join(".fno").join("events.jsonl");
    let global = tmp.join("global-events.jsonl");
    fs::create_dir_all(project.parent().unwrap()).unwrap();
    (Journal::new_raw(project.clone(), global), project)
}

fn journal_events(project_journal: &Path) -> Vec<String> {
    if !project_journal.exists() {
        return vec![];
    }
    fs::read_to_string(project_journal)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|s| s.to_string())
        .collect()
}

fn cfg_for(tmp: &Path, abi_bin: PathBuf, mission: &str) -> DrainConfig {
    DrainConfig {
        cwd: tmp.to_path_buf(),
        abi_bin: abi_bin.display().to_string(),
        mission: mission.to_string(),
        failure_limit: 3,
    }
}

/// A stub `fno` whose `backlog advance` records its argv and prints `receipt` on
/// stdout (exit 0); every other subcommand is a no-op exit 0.
fn stub_advance(dir: &Path, args_file: &Path, receipt: &str) -> PathBuf {
    write_stub(
        dir,
        "fno",
        &format!(
            r#"if [[ "$1" == "backlog" && "$2" == "advance" ]]; then
  echo "$@" > "{a}"
  cat <<'JSON'
{receipt}
JSON
  exit 0
fi
exit 0"#,
            a = args_file.display()
        ),
    )
}

// ── dispatch + argv seam ────────────────────────────────────────────────────────

#[test]
fn dispatch_records_pending_and_forwards_epic_seam() {
    let tmp = TempDir::new().unwrap();
    let args_file = tmp.path().join("advance-args.txt");
    let fno = stub_advance(
        &tmp.path().join("bin"),
        &args_file,
        r#"{"epic_id":"x-epic","deactivated":false,"all_done":false,"dispatched":["x-a","x-b"]}"#,
    );
    let (journal, project_journal) = journal_in(tmp.path());
    let cfg = cfg_for(tmp.path(), fno, "x-epic");
    let mut breaker = CircuitBreaker::new(3);
    let mut pending = Vec::new();

    let out = mission_drain_tick(&cfg, &mut breaker, &mut pending, &journal);
    assert_eq!(out, MissionDispatch::Continue);
    assert_eq!(
        pending.len(),
        2,
        "both dispatched children tracked for reconcile"
    );

    // The converge core is invoked as `advance --epic <mission> --json`.
    let args = fs::read_to_string(&args_file).expect("advance argv recorded");
    assert!(
        args.contains("advance --epic x-epic --json"),
        "epic seam not forwarded: {args}"
    );

    let events = journal_events(&project_journal);
    assert!(
        events
            .iter()
            .any(|e| e.contains("active_backlog_dispatched") && e.contains("x-epic")),
        "expected a mission dispatch event, got: {events:?}"
    );
}

// ── empty scope ─────────────────────────────────────────────────────────────────

#[test]
fn empty_dispatch_records_no_pending() {
    let tmp = TempDir::new().unwrap();
    let args_file = tmp.path().join("advance-args.txt");
    let fno = stub_advance(
        &tmp.path().join("bin"),
        &args_file,
        r#"{"epic_id":"x-epic","deactivated":false,"all_done":false,"dispatched":[]}"#,
    );
    let (journal, project_journal) = journal_in(tmp.path());
    let cfg = cfg_for(tmp.path(), fno, "x-epic");
    let mut breaker = CircuitBreaker::new(3);
    let mut pending = Vec::new();

    let out = mission_drain_tick(&cfg, &mut breaker, &mut pending, &journal);
    assert_eq!(out, MissionDispatch::Continue);
    assert!(pending.is_empty());
    assert!(!journal_events(&project_journal)
        .iter()
        .any(|e| e.contains("active_backlog_dispatched")));
}

// ── retire ──────────────────────────────────────────────────────────────────────

#[test]
fn all_done_retires_the_mission() {
    let tmp = TempDir::new().unwrap();
    let args_file = tmp.path().join("advance-args.txt");
    let fno = stub_advance(
        &tmp.path().join("bin"),
        &args_file,
        r#"{"epic_id":"x-epic","deactivated":false,"all_done":true,"dispatched":[]}"#,
    );
    let (journal, _pj) = journal_in(tmp.path());
    let cfg = cfg_for(tmp.path(), fno, "x-epic");
    let mut breaker = CircuitBreaker::new(3);
    let mut pending = Vec::new();

    let out = mission_drain_tick(&cfg, &mut breaker, &mut pending, &journal);
    assert_eq!(out, MissionDispatch::Retire, "all_done must retire the loop");
}

// ── failure ─────────────────────────────────────────────────────────────────────

#[test]
fn advance_failure_skips_journaled_without_pending() {
    let tmp = TempDir::new().unwrap();
    let fno = write_stub(
        &tmp.path().join("bin"),
        "fno",
        r#"if [[ "$1" == "backlog" && "$2" == "advance" ]]; then echo 'wedged' >&2; exit 3; fi
exit 0"#,
    );
    let (journal, project_journal) = journal_in(tmp.path());
    let cfg = cfg_for(tmp.path(), fno, "x-epic");
    let mut breaker = CircuitBreaker::new(3);
    let mut pending = Vec::new();

    // A non-zero advance is a transient skip (Continue), never a false Retire.
    let out = mission_drain_tick(&cfg, &mut breaker, &mut pending, &journal);
    assert_eq!(out, MissionDispatch::Continue);
    assert!(pending.is_empty());
    assert!(
        journal_events(&project_journal)
            .iter()
            .any(|e| e.contains("advance-epic-failed")),
        "expected an advance-epic-failed skip event"
    );
}
