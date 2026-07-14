//! Integration tests for the active-backlog drain tick.
//!
//! These exercise `drain_tick`'s real IO branches against a stub `fno` binary
//! (passed directly as `DrainConfig.abi_bin`) with the loop Journal pointed at a
//! tempdir so no real `~/.fno` state is touched. Since x-0ad6 the sequential
//! path is fire-and-forget: a tick dispatches via `fno backlog dispatch-lanes
//! --max 1` and reconciles the dispatch from events on a later tick. The
//! reconcile POLICY (event -> breaker success/park, crash floor, in-flight
//! skip) needs seeded `node:<id>` claims and lives in the src module's unit
//! tests (`FNO_CLAIMS_ROOT`-hermetic); this file covers the tick's IO wiring.
//!
//! Coverage map:
//!   AC1-FR   yield to a live manual /megawalk (walker claim held) -> Yielded
//!   (skip)   a walker-claim ERROR (non-1 exit) -> Skipped, never dispatches
//!   AC3-HP   sequential dispatch goes through `dispatch-lanes` (a spawn), no
//!            ShelloutDispatcher child -> Dispatched + one pending recorded
//!   AC1-EDGE empty scope (dispatch-lanes -> []) -> NoWork, no dispatch
//!   parallel mode lane-fill (max_lanes >= 2) is unchanged by the retarget

#![allow(unused_imports)]

use fno_agents::active_backlog::{drain_tick, CircuitBreaker, DrainConfig, DrainOutcome};
use fno_agents::loop_runtime::{GlobalJournalPath, Journal, ProjectJournalPath};
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

/// A driver lib that does nothing (the retargeted sequential path never invokes
/// it - dispatch goes through `dispatch-lanes` - but DrainConfig still requires a
/// path).
fn driver_lib_noop(dir: &Path) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let p = dir.join("driver-claude-code.sh");
    fs::write(
        &p,
        b"#!/usr/bin/env bash\ndriver_default_max() { echo 10; }\ndriver_invoke() { exit 0; }\n",
    )
    .unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p
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

fn base_cfg(tmp: &Path, abi_bin: PathBuf, lib_path: PathBuf, failure_limit: u32) -> DrainConfig {
    DrainConfig {
        cwd: tmp.to_path_buf(),
        project: Some("footnote".to_string()),
        mission: None,
        lib_path,
        abi_bin: abi_bin.display().to_string(),
        allow_merge: false,
        max_turns: 5,
        budget_usd: 25.0,
        model: None,
        max_iterations: 10,
        per_unit_max_dispatches: 1,
        failure_limit,
        batch: false,
        max_lanes: 1,
    }
}

// ── AC1-FR: yield to a live manual /megawalk ────────────────────────────────────

#[test]
fn ac1_fr_yields_when_walker_claim_held() {
    let tmp = TempDir::new().unwrap();
    let bin = tmp.path().join("bin");
    // Every `claim acquire` exits 1 (held by another walker).
    let fno = write_stub(
        &bin,
        "fno",
        r#"if [[ "$1" == "claim" && "$2" == "acquire" ]]; then exit 1; fi
exit 0"#,
    );
    let lib = driver_lib_noop(&tmp.path().join("lib"));
    let (journal, project_journal) = journal_in(tmp.path());
    let cfg = base_cfg(tmp.path(), fno, lib, 3);
    let mut breaker = CircuitBreaker::new(3);
    let mut pending = Vec::new();

    let out = drain_tick(&cfg, &mut breaker, &mut pending, &journal);
    assert_eq!(out, DrainOutcome::Yielded);
    let events = journal_events(&project_journal);
    assert!(
        events
            .iter()
            .any(|e| e.contains("active_backlog_yield") && e.contains("walker-live")),
        "expected active_backlog_yield event, got: {events:?}"
    );
    assert!(!events
        .iter()
        .any(|e| e.contains("active_backlog_dispatched")));
    assert!(pending.is_empty(), "a yield must not record a dispatch");
}

// ── skip: walker-claim error (non-1 exit) ───────────────────────────────────────

#[test]
fn walker_claim_error_skips_without_dispatch() {
    let tmp = TempDir::new().unwrap();
    let bin = tmp.path().join("bin");
    // claim acquire exits 2 (a real error, not "held"): the tick must skip, never
    // dispatch.
    let fno = write_stub(
        &bin,
        "fno",
        r#"if [[ "$1" == "claim" && "$2" == "acquire" ]]; then exit 2; fi
exit 0"#,
    );
    let lib = driver_lib_noop(&tmp.path().join("lib"));
    let (journal, project_journal) = journal_in(tmp.path());
    let cfg = base_cfg(tmp.path(), fno, lib, 3);
    let mut breaker = CircuitBreaker::new(3);
    let mut pending = Vec::new();

    let out = drain_tick(&cfg, &mut breaker, &mut pending, &journal);
    assert!(matches!(out, DrainOutcome::Skipped { .. }), "got {out:?}");
    let events = journal_events(&project_journal);
    assert!(events.iter().any(|e| e.contains("active_backlog_skip")));
    assert!(!events
        .iter()
        .any(|e| e.contains("active_backlog_dispatched")));
    assert!(pending.is_empty());
}

// ── AC3-HP: sequential dispatch goes through dispatch-lanes (a spawn) ────────────

#[test]
fn ac3_hp_sequential_dispatch_via_lanes_records_pending() {
    let tmp = TempDir::new().unwrap();
    let bin = tmp.path().join("bin");
    // The retargeted sequential tick fire-and-forgets ONE node via
    // `dispatch-lanes --max 1` (a `fno agents spawn`, no ShelloutDispatcher
    // child). The stub records its argv so the --max/--project seam is checked.
    let args_file = tmp.path().join("dispatch-args.txt");
    let fno = write_stub(
        &bin,
        "fno",
        &format!(
            r#"if [[ "$1" == "backlog" && "$2" == "dispatch-lanes" ]]; then
  echo "$@" > "{a}"
  echo '[{{"node_id":"x-seq0001","status":"dispatched","short_id":"s1"}}]'
  exit 0
fi
exit 0"#,
            a = args_file.display()
        ),
    );
    let lib = driver_lib_noop(&tmp.path().join("lib"));
    let (journal, project_journal) = journal_in(tmp.path());
    let cfg = base_cfg(tmp.path(), fno, lib, 3);
    let mut breaker = CircuitBreaker::new(3);
    let mut pending = Vec::new();

    let out = drain_tick(&cfg, &mut breaker, &mut pending, &journal);
    assert_eq!(
        out,
        DrainOutcome::Dispatched {
            node: "x-seq0001".to_string()
        },
        "got {out:?}"
    );
    // The dispatch is recorded for a later reconcile tick.
    assert_eq!(pending.len(), 1, "one in-flight dispatch must be tracked");
    // Sequential cap crosses the seam.
    let args = fs::read_to_string(&args_file).expect("dispatch-lanes argv recorded");
    assert!(
        args.contains("--max 1"),
        "sequential cap not forwarded: {args}"
    );
    assert!(
        args.contains("--project footnote"),
        "project scope not forwarded: {args}"
    );
    let events = journal_events(&project_journal);
    assert!(
        events
            .iter()
            .any(|e| e.contains("active_backlog_dispatched") && e.contains("fire_and_forget")),
        "expected a fire-and-forget dispatch event, got: {events:?}"
    );
}

// ── AC1-EDGE: empty scope ───────────────────────────────────────────────────────

#[test]
fn ac1_edge_no_work_when_nothing_to_dispatch() {
    let tmp = TempDir::new().unwrap();
    let bin = tmp.path().join("bin");
    // dispatch-lanes selects nothing (drained board) -> NoWork, nothing pending.
    let fno = write_stub(
        &bin,
        "fno",
        r#"if [[ "$1" == "backlog" && "$2" == "dispatch-lanes" ]]; then echo '[]'; exit 0; fi
exit 0"#,
    );
    let lib = driver_lib_noop(&tmp.path().join("lib"));
    let (journal, project_journal) = journal_in(tmp.path());
    let cfg = base_cfg(tmp.path(), fno, lib, 3);
    let mut breaker = CircuitBreaker::new(3);
    let mut pending = Vec::new();

    let out = drain_tick(&cfg, &mut breaker, &mut pending, &journal);
    assert_eq!(out, DrainOutcome::NoWork);
    assert!(pending.is_empty());
    let events = journal_events(&project_journal);
    assert!(!events
        .iter()
        .any(|e| e.contains("active_backlog_dispatched")));
}

// ── parallel mode (x-42d5 G4): lane-fill tick (unchanged by the retarget) ────────

#[test]
fn parallel_tick_dispatches_lanes_and_journals() {
    let tmp = TempDir::new().unwrap();
    let bin = tmp.path().join("bin");
    let args_file = tmp.path().join("dispatch-args.txt");
    let fno = write_stub(
        &bin,
        "fno",
        &format!(
            r#"if [[ "$1" == "backlog" && "$2" == "dispatch-lanes" ]]; then
  echo "$@" > "{a}"
  echo '[{{"node_id":"x-a","status":"dispatched","short_id":"s1"}},{{"node_id":"x-b","status":"skipped","error":"spawn rc=127"}}]'
  exit 0
fi
exit 0"#,
            a = args_file.display()
        ),
    );
    let lib = driver_lib_noop(&tmp.path().join("lib"));
    let (journal, project_journal) = journal_in(tmp.path());
    let mut cfg = base_cfg(tmp.path(), fno, lib, 3);
    cfg.max_lanes = 3;
    cfg.mission = Some("m-7".to_string());
    let mut breaker = CircuitBreaker::new(3);
    let mut pending = Vec::new();

    let out = drain_tick(&cfg, &mut breaker, &mut pending, &journal);
    assert_eq!(
        out,
        DrainOutcome::LanesDispatched {
            dispatched: 1,
            skipped: 1
        }
    );
    // Parallel lanes close at merge via reconcile, not through the sequential
    // pending set.
    assert!(pending.is_empty());
    let events = journal_events(&project_journal);
    assert!(
        events
            .iter()
            .any(|e| e.contains("active_backlog_dispatched") && e.contains("max_lanes")),
        "expected a lanes dispatch event, got: {events:?}"
    );
    let args = fs::read_to_string(&args_file).expect("dispatch-lanes argv recorded");
    assert!(args.contains("--max 3"), "cap not forwarded: {args}");
    assert!(
        args.contains("--project footnote"),
        "project scope not forwarded: {args}"
    );
    assert!(
        args.contains("--mission m-7"),
        "mission scope not forwarded: {args}"
    );
}

#[test]
fn parallel_tick_empty_selection_is_no_work() {
    let tmp = TempDir::new().unwrap();
    let bin = tmp.path().join("bin");
    let fno = write_stub(
        &bin,
        "fno",
        r#"if [[ "$1" == "backlog" && "$2" == "dispatch-lanes" ]]; then echo '[]'; exit 0; fi
exit 0"#,
    );
    let lib = driver_lib_noop(&tmp.path().join("lib"));
    let (journal, project_journal) = journal_in(tmp.path());
    let mut cfg = base_cfg(tmp.path(), fno, lib, 3);
    cfg.max_lanes = 2;
    let mut breaker = CircuitBreaker::new(3);
    let mut pending = Vec::new();

    let out = drain_tick(&cfg, &mut breaker, &mut pending, &journal);
    assert_eq!(out, DrainOutcome::NoWork);
    let events = journal_events(&project_journal);
    assert!(!events
        .iter()
        .any(|e| e.contains("active_backlog_dispatched")));
}

#[test]
fn parallel_tick_dispatch_failure_skips_and_journals() {
    let tmp = TempDir::new().unwrap();
    let bin = tmp.path().join("bin");
    let fno = write_stub(
        &bin,
        "fno",
        r#"if [[ "$1" == "backlog" && "$2" == "dispatch-lanes" ]]; then echo 'wedged' >&2; exit 3; fi
exit 0"#,
    );
    let lib = driver_lib_noop(&tmp.path().join("lib"));
    let (journal, project_journal) = journal_in(tmp.path());
    let mut cfg = base_cfg(tmp.path(), fno, lib, 3);
    cfg.max_lanes = 2;
    let mut breaker = CircuitBreaker::new(3);
    let mut pending = Vec::new();

    let out = drain_tick(&cfg, &mut breaker, &mut pending, &journal);
    assert!(matches!(out, DrainOutcome::Skipped { .. }), "got {out:?}");
    let events = journal_events(&project_journal);
    assert!(
        events.iter().any(|e| e.contains("dispatch-lanes-failed")),
        "expected a dispatch-lanes-failed skip event, got: {events:?}"
    );
}
