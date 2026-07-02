//! Integration tests for the active-backlog drain tick (node x-c070, Wave 5).
//!
//! These exercise `drain_tick`'s real IO branches against a stub `fno` binary
//! (passed directly as `DrainConfig.abi_bin`) and a stub driver lib, with the
//! loop Journal pointed at a tempdir so no real `~/.fno` state is touched.
//!
//! Acceptance-criteria coverage map:
//!   AC1-FR   yield to a live manual /megawalk (walker claim held) -> Yielded
//!   AC1-EDGE empty / drained scope (backlog next -> null) -> NoWork, no dispatch
//!   AC1-HP   a node the worker completes -> Dispatched + breaker reset
//!   AC2-FR   a crash-looping node parks after failure_limit ticks -> Parked
//!   (skip)   a walker-claim ERROR (non-1 exit) -> Skipped, never dispatches
//!
//! AC2-FR's breaker *policy* is also unit-tested in src/active_backlog.rs; these
//! tests prove the drain_tick wiring drives it. AC3-FR (poll+nudge double-fire)
//! is enforced by `fno backlog next`'s live-claims filter (Python-side tests).
//! AC2-EDGE (disable mid-flight) is covered by the Python resolver tests.

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

fn node_json(id: &str) -> String {
    format!(
        r#"{{
  "id": "{id}",
  "title": "Drain {id}",
  "priority": "p2",
  "domain": "code",
  "project": "footnote",
  "cwd": "/tmp/x",
  "size": null,
  "plan_path": null
}}"#
    )
}

/// A driver lib whose `driver_invoke` writes a DonePRGreen termination event for
/// the dispatched session, so run_loop closes the unit successfully.
fn driver_lib_done(dir: &Path, journal: &Path) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let body = format!(
        r#"#!/usr/bin/env bash
driver_default_max() {{ echo 10; }}
driver_invoke() {{
  local sid="${{TARGET_SESSION_ID:-}}"
  local ts
  ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  printf '{{"ts":"%s","type":"termination","source":"hook","data":{{"session_id":"%s","reason":"DonePRGreen","message":"done"}}}}\n' "$ts" "$sid" >> "{j}"
}}
"#,
        j = journal.display()
    );
    let p = dir.join("driver-claude-code.sh");
    fs::write(&p, body.as_bytes()).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p
}

/// A driver lib whose `driver_invoke` does nothing (never emits a termination),
/// so run_loop synthesizes NoProgress and parks the unit.
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

    let out = drain_tick(&cfg, &mut breaker, &journal);
    assert_eq!(out, DrainOutcome::Yielded);
    let events = journal_events(&project_journal);
    assert!(
        events
            .iter()
            .any(|e| e.contains("active_backlog_yield") && e.contains("walker-live")),
        "expected active_backlog_yield event, got: {events:?}"
    );
    // No dispatch happened.
    assert!(!events
        .iter()
        .any(|e| e.contains("active_backlog_dispatched")));
}

// ── AC1-EDGE: empty / drained scope ─────────────────────────────────────────────

#[test]
fn ac1_edge_no_work_when_backlog_empty() {
    let tmp = TempDir::new().unwrap();
    let bin = tmp.path().join("bin");
    // Walker claim succeeds; backlog next returns null (drained); release ok.
    let fno = write_stub(
        &bin,
        "fno",
        r#"if [[ "$1" == "backlog" && "$2" == "next" ]]; then echo 'null'; exit 0; fi
exit 0"#,
    );
    let lib = driver_lib_noop(&tmp.path().join("lib"));
    let (journal, project_journal) = journal_in(tmp.path());
    let cfg = base_cfg(tmp.path(), fno, lib, 3);
    let mut breaker = CircuitBreaker::new(3);

    let out = drain_tick(&cfg, &mut breaker, &journal);
    assert_eq!(out, DrainOutcome::NoWork);
    // No dispatch event for an empty board.
    let events = journal_events(&project_journal);
    assert!(!events
        .iter()
        .any(|e| e.contains("active_backlog_dispatched")));
    assert!(!events.iter().any(|e| e.contains("active_backlog_parked")));
}

// ── skip: walker-claim error (non-1 exit) ───────────────────────────────────────

#[test]
fn walker_claim_error_skips_without_dispatch() {
    let tmp = TempDir::new().unwrap();
    let bin = tmp.path().join("bin");
    // claim acquire exits 2 (a real error, not "held"): the tick must skip, never
    // guess a node, never dispatch.
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

    let out = drain_tick(&cfg, &mut breaker, &journal);
    assert!(matches!(out, DrainOutcome::Skipped { .. }), "got {out:?}");
    let events = journal_events(&project_journal);
    assert!(events.iter().any(|e| e.contains("active_backlog_skip")));
    assert!(!events
        .iter()
        .any(|e| e.contains("active_backlog_dispatched")));
}

// ── AC1-HP: a node the worker completes -> Dispatched ───────────────────────────

#[test]
fn ac1_hp_dispatches_and_closes_a_completed_node() {
    let tmp = TempDir::new().unwrap();
    let bin = tmp.path().join("bin");
    let (journal, project_journal) = journal_in(tmp.path());
    // The driver writes its termination event into the SAME project journal that
    // run_loop reads via find_termination.
    let lib = driver_lib_done(&tmp.path().join("lib"), &project_journal);
    // fno: walker/node claim ok, backlog next -> node A then null, done ok.
    let node_a = node_json("ab-hpaaaaaa");
    let fno = write_stub(
        &bin,
        "fno",
        &format!(
            r#"if [[ "$1" == "backlog" && "$2" == "next" ]]; then
  c="$(cat "{cnt}" 2>/dev/null || echo 0)"; c=$((c+1)); echo "$c" > "{cnt}"
  if [[ "$c" -eq 1 ]]; then echo '{node_a}'; else echo 'null'; fi
  exit 0
fi
exit 0"#,
            cnt = tmp.path().join("cnt.txt").display(),
            node_a = node_a,
        ),
    );
    fs::write(tmp.path().join("cnt.txt"), "0").unwrap();
    let cfg = base_cfg(tmp.path(), fno, lib, 3);
    let mut breaker = CircuitBreaker::new(3);
    breaker.record_failure("ab-hpaaaaaa"); // pre-existing streak to prove reset

    let out = drain_tick(&cfg, &mut breaker, &journal);
    assert_eq!(
        out,
        DrainOutcome::Dispatched {
            node: "ab-hpaaaaaa".to_string()
        }
    );
    // Success resets the breaker streak for that node.
    assert_eq!(breaker.consecutive_failures("ab-hpaaaaaa"), 0);
    let events = journal_events(&project_journal);
    assert!(events
        .iter()
        .any(|e| e.contains("active_backlog_dispatched") && e.contains("ab-hpaaaaaa")));
}

// ── AC2-FR: crash-loop park after failure_limit ticks ───────────────────────────

#[test]
fn ac2_fr_crash_loop_parks_after_failure_limit() {
    let tmp = TempDir::new().unwrap();
    let bin = tmp.path().join("bin");
    // No-op driver: the worker never emits a termination, so run_loop synthesizes
    // NoProgress (per_unit_max_dispatches=1) and parks the unit each tick.
    let lib = driver_lib_noop(&tmp.path().join("lib"));
    // backlog next ALWAYS returns the same node (the stub has no live-claims
    // filter), so the daemon-side breaker is what eventually parks it.
    let node_a = node_json("ab-park0001");
    let fno = write_stub(
        &bin,
        "fno",
        &format!(
            r#"if [[ "$1" == "backlog" && "$2" == "next" ]]; then echo '{node_a}'; exit 0; fi
exit 0"#,
            node_a = node_a,
        ),
    );
    let (journal, project_journal) = journal_in(tmp.path());
    let cfg = base_cfg(tmp.path(), fno, lib, 3);
    let mut breaker = CircuitBreaker::new(3);

    // Ticks 1 and 2: the node fails but has not yet hit the limit -> Skipped.
    for i in 1u32..=2 {
        let out = drain_tick(&cfg, &mut breaker, &journal);
        assert!(
            matches!(out, DrainOutcome::Skipped { .. }),
            "tick {i}: got {out:?}"
        );
        assert_eq!(
            breaker.consecutive_failures("ab-park0001"),
            i,
            "tick {i}: streak should be {i}"
        );
    }
    // Tick 3 trips the breaker -> Parked (node deferred, streak reset).
    let out = drain_tick(&cfg, &mut breaker, &journal);
    assert_eq!(
        out,
        DrainOutcome::Parked {
            node: "ab-park0001".to_string(),
            failures: 3
        }
    );
    // After a trip the streak is reset (the deferred graph state owns exclusion,
    // and a later `fno backlog undefer` gives a fresh failure_limit attempts).
    assert_eq!(breaker.consecutive_failures("ab-park0001"), 0);
    let events = journal_events(&project_journal);
    assert!(
        events
            .iter()
            .any(|e| e.contains("active_backlog_parked") && e.contains("ab-park0001")),
        "expected active_backlog_parked event"
    );
}

// ── batch-lane (x-6cdf): batched dispatch when config.batch is on ────────────────

/// A driver that records CONTINUE_PROMPT + its cwd + batch env to a file, then
/// emits a `reason` termination so run_loop closes the unit. `reason` lets the
/// batched path emit DoneBatched and the non-batched path emit DonePRGreen.
fn driver_lib_record_reason(dir: &Path, journal: &Path, record: &Path, reason: &str) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let body = format!(
        r#"#!/usr/bin/env bash
driver_default_max() {{ echo 10; }}
driver_invoke() {{
  local sid="${{TARGET_SESSION_ID:-}}"
  printf 'prompt=%s\ncwd=%s\nbatched=%s\nworktree=%s\n' \
    "${{CONTINUE_PROMPT:-}}" "$PWD" "${{TARGET_BATCHED:-}}" "${{TARGET_BATCH_WORKTREE:-}}" > "{rec}"
  local ts; ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  printf '{{"ts":"%s","type":"termination","source":"hook","data":{{"session_id":"%s","reason":"{reason}","message":"m"}}}}\n' "$ts" "$sid" >> "{j}"
}}
"#,
        rec = record.display(),
        j = journal.display(),
        reason = reason,
    );
    let p = dir.join("driver-claude-code.sh");
    fs::write(&p, body.as_bytes()).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p
}

#[test]
fn batch_enabled_dispatches_batched_worker_and_ships_closeable() {
    let tmp = TempDir::new().unwrap();
    let bin = tmp.path().join("bin");
    let (journal, project_journal) = journal_in(tmp.path());
    let record = tmp.path().join("driver-record.txt");
    let lib = driver_lib_record_reason(
        &tmp.path().join("lib"),
        &project_journal,
        &record,
        "DoneBatched",
    );

    // The shared batch worktree must exist (ShelloutDispatcher runs cwd there).
    let wt = tmp.path().join("batch-wt");
    fs::create_dir_all(&wt).unwrap();

    // fno stub: backlog next -> node A then null; `batch prepare` -> batched with
    // the temp worktree; `batch ship-closeable` -> records it fired; else ok.
    let ship_marker = tmp.path().join("ship-closeable-fired");
    let node_a = node_json("ab-batch001");
    let fno = write_stub(
        &bin,
        "fno",
        &format!(
            r#"if [[ "$1" == "backlog" && "$2" == "next" ]]; then
  c="$(cat "{cnt}" 2>/dev/null || echo 0)"; c=$((c+1)); echo "$c" > "{cnt}"
  if [[ "$c" -eq 1 ]]; then echo '{node_a}'; else echo 'null'; fi
  exit 0
fi
if [[ "$1" == "backlog" && "$2" == "batch" && "$3" == "prepare" ]]; then
  printf '{{"mode":"batched","domain":"code","worktree":"{wt}","branch":"feature/batch-code","batch_id":"batch-aaaa"}}\n'
  exit 0
fi
if [[ "$1" == "backlog" && "$2" == "batch" && "$3" == "ship-closeable" ]]; then
  echo fired > "{ship}"; printf '{{"shipped":[]}}\n'; exit 0
fi
exit 0"#,
            cnt = tmp.path().join("cnt.txt").display(),
            node_a = node_a,
            wt = wt.display(),
            ship = ship_marker.display(),
        ),
    );
    fs::write(tmp.path().join("cnt.txt"), "0").unwrap();

    let mut cfg = base_cfg(tmp.path(), fno, lib, 3);
    cfg.batch = true;
    let mut breaker = CircuitBreaker::new(3);

    let out = drain_tick(&cfg, &mut breaker, &journal);
    assert_eq!(
        out,
        DrainOutcome::Dispatched {
            node: "ab-batch001".to_string()
        },
        "got {out:?}"
    );

    // The worker was dispatched as a batched /target in the shared worktree.
    let rec = fs::read_to_string(&record).unwrap();
    assert!(
        rec.contains("prompt=/target batched ab-batch001"),
        "record: {rec}"
    );
    assert!(
        rec.contains("batched=1"),
        "TARGET_BATCHED must be set: {rec}"
    );
    assert!(
        rec.contains(&format!("worktree={}", wt.display())),
        "record: {rec}"
    );
    // cwd ends with the batch worktree dir (macOS /var -> /private/var symlink
    // normalization means an exact prefix match is brittle).
    assert!(
        rec.lines()
            .any(|l| l.starts_with("cwd=") && l.ends_with("batch-wt")),
        "driver must run in batch worktree: {rec}"
    );

    // ship-closeable fired after the tick.
    assert!(
        ship_marker.exists(),
        "ship-closeable must be invoked when batch is on"
    );
}

#[test]
fn batch_disabled_dispatches_normal_worker() {
    // With cfg.batch=false the dispatch is byte-for-byte today's path: no
    // `batch prepare` consult, prompt is `/target no-merge <id>`.
    let tmp = TempDir::new().unwrap();
    let bin = tmp.path().join("bin");
    let (journal, project_journal) = journal_in(tmp.path());
    let record = tmp.path().join("driver-record.txt");
    let lib = driver_lib_record_reason(
        &tmp.path().join("lib"),
        &project_journal,
        &record,
        "DonePRGreen",
    );
    let node_a = node_json("ab-nobatch1");
    let fno = write_stub(
        &bin,
        "fno",
        &format!(
            r#"if [[ "$1" == "backlog" && "$2" == "next" ]]; then
  c="$(cat "{cnt}" 2>/dev/null || echo 0)"; c=$((c+1)); echo "$c" > "{cnt}"
  if [[ "$c" -eq 1 ]]; then echo '{node_a}'; else echo 'null'; fi
  exit 0
fi
if [[ "$1" == "backlog" && "$2" == "batch" ]]; then echo 'UNEXPECTED batch call' >&2; exit 99; fi
exit 0"#,
            cnt = tmp.path().join("cnt.txt").display(),
            node_a = node_a,
        ),
    );
    fs::write(tmp.path().join("cnt.txt"), "0").unwrap();
    let cfg = base_cfg(tmp.path(), fno, lib, 3); // batch defaults false
    let mut breaker = CircuitBreaker::new(3);

    let out = drain_tick(&cfg, &mut breaker, &journal);
    assert_eq!(
        out,
        DrainOutcome::Dispatched {
            node: "ab-nobatch1".to_string()
        },
        "got {out:?}"
    );
    let rec = fs::read_to_string(&record).unwrap();
    assert!(
        rec.contains("prompt=/target no-merge ab-nobatch1"),
        "record: {rec}"
    );
    assert!(
        rec.contains("batched=\n") || !rec.contains("batched=1"),
        "TARGET_BATCHED unset: {rec}"
    );
}

#[test]
fn batched_member_failure_abandons_batch_and_does_not_ship() {
    // A batched member that fails (no termination -> NoProgress) must ABANDON its
    // batch (v1 policy), never run ship-closeable over its partial commits.
    let tmp = TempDir::new().unwrap();
    let bin = tmp.path().join("bin");
    let (journal, _project_journal) = journal_in(tmp.path());
    // noop driver: never emits a termination -> run_loop synthesizes NoProgress.
    let lib = driver_lib_noop(&tmp.path().join("lib"));
    let wt = tmp.path().join("batch-wt");
    fs::create_dir_all(&wt).unwrap();
    let abandon_marker = tmp.path().join("abandon-fired");
    let ship_marker = tmp.path().join("ship-closeable-fired");
    let node_a = node_json("ab-batchfail");
    let fno = write_stub(
        &bin,
        "fno",
        &format!(
            r#"if [[ "$1" == "backlog" && "$2" == "next" ]]; then
  c="$(cat "{cnt}" 2>/dev/null || echo 0)"; c=$((c+1)); echo "$c" > "{cnt}"
  if [[ "$c" -eq 1 ]]; then echo '{node_a}'; else echo 'null'; fi
  exit 0
fi
if [[ "$1" == "backlog" && "$2" == "batch" && "$3" == "prepare" ]]; then
  printf '{{"mode":"batched","domain":"code","worktree":"{wt}","branch":"feature/batch-code-aa11bb","batch_id":"batch-bbbb"}}\n'
  exit 0
fi
if [[ "$1" == "backlog" && "$2" == "batch" && "$3" == "abandon" ]]; then echo abandoned > "{ab}"; printf '{{}}\n'; exit 0; fi
if [[ "$1" == "backlog" && "$2" == "batch" && "$3" == "ship-closeable" ]]; then echo fired > "{ship}"; exit 0; fi
exit 0"#,
            cnt = tmp.path().join("cnt.txt").display(),
            node_a = node_a,
            wt = wt.display(),
            ab = abandon_marker.display(),
            ship = ship_marker.display(),
        ),
    );
    fs::write(tmp.path().join("cnt.txt"), "0").unwrap();
    let mut cfg = base_cfg(tmp.path(), fno, lib, 3);
    cfg.batch = true;
    let mut breaker = CircuitBreaker::new(3);

    let _out = drain_tick(&cfg, &mut breaker, &journal);
    assert!(
        abandon_marker.exists(),
        "a failed batched member must abandon its batch"
    );
    assert!(
        !ship_marker.exists(),
        "ship-closeable must NOT run after a batched member failure"
    );
}

#[test]
fn batch_drain_tick_ships_closeable_with_no_unit() {
    // A NoWork tick (backlog drained: all batched members filtered out of `next`)
    // must still run ship-closeable so the drain condition can open the open
    // batch's PR. Gating ship-closeable on a DoneBatched unit would strand it.
    let tmp = TempDir::new().unwrap();
    let bin = tmp.path().join("bin");
    let (journal, _pj) = journal_in(tmp.path());
    let lib = driver_lib_noop(&tmp.path().join("lib"));
    let ship_marker = tmp.path().join("ship-closeable-fired");
    let fno = write_stub(
        &bin,
        "fno",
        &format!(
            r#"if [[ "$1" == "backlog" && "$2" == "next" ]]; then echo 'null'; exit 0; fi
if [[ "$1" == "backlog" && "$2" == "batch" && "$3" == "ship-closeable" ]]; then echo fired > "{ship}"; printf '{{"shipped":[]}}\n'; exit 0; fi
exit 0"#,
            ship = ship_marker.display(),
        ),
    );
    let mut cfg = base_cfg(tmp.path(), fno, lib, 3);
    cfg.batch = true;
    let mut breaker = CircuitBreaker::new(3);

    let out = drain_tick(&cfg, &mut breaker, &journal);
    assert_eq!(out, DrainOutcome::NoWork);
    assert!(
        ship_marker.exists(),
        "ship-closeable must run on a drained (NoWork) tick to close an open batch"
    );
}

// ── parallel mode (x-42d5 G4): lane-fill tick ───────────────────────────────────

#[test]
fn parallel_tick_dispatches_lanes_and_journals() {
    let tmp = TempDir::new().unwrap();
    let bin = tmp.path().join("bin");
    // Walker claim succeeds; dispatch-lanes fire-and-forgets two picks, one of
    // which fails to spawn (contained per-lane: receipt status "skipped").
    // The stub records its argv: the cap and project scope MUST cross the
    // Rust->Python seam, else the daemon lane-fills the wrong scope.
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
    let mut breaker = CircuitBreaker::new(3);

    let out = drain_tick(&cfg, &mut breaker, &journal);
    assert_eq!(
        out,
        DrainOutcome::LanesDispatched {
            dispatched: 1,
            skipped: 1
        }
    );
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

    let out = drain_tick(&cfg, &mut breaker, &journal);
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

    let out = drain_tick(&cfg, &mut breaker, &journal);
    assert!(matches!(out, DrainOutcome::Skipped { .. }), "got {out:?}");
    let events = journal_events(&project_journal);
    assert!(
        events.iter().any(|e| e.contains("dispatch-lanes-failed")),
        "expected a dispatch-lanes-failed skip event, got: {events:?}"
    );
}

#[test]
fn max_lanes_one_never_calls_dispatch_lanes() {
    // AC1-EDGE: max_lanes == 1 is byte-identical sequential behavior - the
    // lane dispatcher is never consulted (a call would trip the stub's marker).
    let tmp = TempDir::new().unwrap();
    let bin = tmp.path().join("bin");
    let marker = tmp.path().join("lanes-called");
    let fno = write_stub(
        &bin,
        "fno",
        &format!(
            r#"if [[ "$1" == "backlog" && "$2" == "dispatch-lanes" ]]; then touch "{m}"; exit 0; fi
if [[ "$1" == "backlog" && "$2" == "next" ]]; then echo 'null'; exit 0; fi
exit 0"#,
            m = marker.display()
        ),
    );
    let lib = driver_lib_noop(&tmp.path().join("lib"));
    let (journal, _project_journal) = journal_in(tmp.path());
    let cfg = base_cfg(tmp.path(), fno, lib, 3); // max_lanes: 1
    let mut breaker = CircuitBreaker::new(3);

    let out = drain_tick(&cfg, &mut breaker, &journal);
    assert_eq!(out, DrainOutcome::NoWork);
    assert!(
        !marker.exists(),
        "sequential tick must not consult dispatch-lanes"
    );
}
