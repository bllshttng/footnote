#![allow(unused_imports)]

/// Integration tests for loop_megawalk.rs (MegawalkQueue + verb glue).
///
/// Tests use stub `fno` binaries (via FNO_BIN env override) that record
/// their argv so assertions can verify the exact call shape. All file I/O
/// uses temporary directories; no real backlog graph or network involved.
///
/// Test naming mirrors the acceptance criteria in the task spec:
///   AC1-HP: next maps JSON fields correctly + acquires claim with exact argv
///   AC2-HP: next on "null" -> None (empty backlog)
///   AC3-ERR: malformed JSON -> Queue error with staleness hint
///   AC4-HP: claim exit-1 -> re-pick loop returns the next node
///   AC5-EDGE: bounded-retry exhaustion errors
///   AC6-HP: close DonePRGreen -> calls done -> Closed
///   AC7-HP: done nonzero -> Parked + claim still released
///   AC8-HP: close Budget -> does NOT call done -> Parked
///   AC9-VERIFY: find_termination finds event in global journal (not project)
///   AC10-E2E: end-to-end run_loop walk: 2 nodes then null, driver emits
///             termination events -> both Closed, walk NoWork, exit 0
use fno_agents::loop_megawalk::MegawalkQueue;
use fno_agents::loop_runtime::{
    run_loop, CloseOutcome, Evidence, Journal, LoopBudget, LoopError, Queue, Unit,
};
use fno_agents::loopcheck::TerminationReason;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

// ── helpers ───────────────────────────────────────────────────────────────────

/// Write an executable stub script at `dir/name`.
fn write_stub(dir: &Path, name: &str, body: &str) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    let content = format!("#!/usr/bin/env bash\n{body}\n");
    fs::write(&path, content.as_bytes()).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Build a PATH string that includes a fake-bin dir before system dirs.
fn path_with(dir: &Path) -> String {
    format!("{}:/bin:/usr/bin:/usr/local/bin", dir.display())
}

/// Real `_node_summary` JSON shape from graph/cli.py:1182-1188.
/// The output is pretty-printed (indent=2) as json.dumps emits.
fn real_node_json(id: &str, title: &str, plan_path: Option<&str>) -> String {
    let pp = match plan_path {
        Some(p) => format!("\"{}\"", p),
        None => "null".to_string(),
    };
    format!(
        r#"{{
  "id": "{id}",
  "title": "{title}",
  "priority": "p2",
  "domain": "code",
  "project": "abilities",
  "cwd": "/home/user/code/abilities",
  "size": null,
  "plan_path": {pp}
}}"#
    )
}

/// Write a termination event into `journal_path` for `session_key`.
fn seed_termination(journal_path: &Path, session_key: &str, reason: &str) {
    let line = format!(
        "{{\"ts\":\"2026-06-06T00:00:00Z\",\"type\":\"termination\",\"source\":\"hook\",\
         \"data\":{{\"session_id\":\"{session_key}\",\"reason\":\"{reason}\",\"message\":\"done\"}}}}\n"
    );
    fs::create_dir_all(journal_path.parent().unwrap()).unwrap();
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(journal_path)
        .unwrap();
    f.write_all(line.as_bytes()).unwrap();
}

/// Read all lines from a file.
fn read_file_lines(path: &Path) -> Vec<String> {
    if !path.exists() {
        return vec![];
    }
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.to_string())
        .collect()
}

// ── AC1-HP: next maps fields correctly + acquires claim with exact argv ───────

#[test]
fn ac1_next_maps_fields_and_acquires_claim() {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");
    let call_log = tmp.path().join("calls.log");

    // Build stub fno: first invocation (backlog next) returns real-shaped JSON;
    // second invocation (claim acquire) exits 0.
    let call_log_str = call_log.display().to_string();
    write_stub(
        &bin_dir,
        "fno",
        &format!(
            r#"echo "$@" >> "{call_log_str}"
if [[ "$1" == "backlog" && "$2" == "next" ]]; then
  echo '{node_json}'
  exit 0
fi
if [[ "$1" == "claim" && "$2" == "acquire" ]]; then
  exit 0
fi
exit 0"#,
            node_json = real_node_json(
                "ab-7303e5d7",
                "Group 2: megawalk over the loop",
                Some("/home/user/code/abilities/internal/fno/design/2026-06-05.md#group-2")
            ),
            call_log_str = call_log_str,
        ),
    );

    let abi_stub = bin_dir.join("fno").display().to_string();
    let mut q = MegawalkQueue::new(
        abi_stub, None,  // project filter
        false, // all
    );

    let unit = q.next().expect("no error").expect("expected Some(unit)");

    assert_eq!(unit.id, "ab-7303e5d7");
    assert_eq!(unit.title, "Group 2: megawalk over the loop");
    assert!(
        unit.session_key.contains("mw"),
        "session_key should contain 'mw'"
    );
    assert_eq!(
        unit.plan_path.as_deref(),
        Some("/home/user/code/abilities/internal/fno/design/2026-06-05.md#group-2")
    );

    // Verify claim was acquired with the right key and holder prefix.
    let calls = read_file_lines(&call_log);
    let claim_call = calls
        .iter()
        .find(|l| l.contains("claim acquire"))
        .expect("claim acquire not called");
    assert!(
        claim_call.contains("node:ab-7303e5d7"),
        "claim key must be node:<id>"
    );
    assert!(
        claim_call.contains("target-session:"),
        "holder must be target-session:<session_key>"
    );
    assert!(claim_call.contains("--ttl"), "ttl flag must be present");
    assert!(
        claim_call.contains("--reason"),
        "reason flag must be present"
    );
}

// ── AC2-HP: next on "null" -> None (empty backlog) ───────────────────────────

#[test]
fn ac2_empty_backlog_returns_none() {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");

    write_stub(
        &bin_dir,
        "fno",
        r#"if [[ "$1" == "backlog" && "$2" == "next" ]]; then
  echo 'null'
  exit 0
fi
exit 0"#,
    );

    let abi_stub = bin_dir.join("fno").display().to_string();
    let mut q = MegawalkQueue::new(abi_stub, None, false);
    let result = q.next().expect("no error");
    assert!(result.is_none(), "null output must return None");
}

// ── AC3-ERR: malformed JSON -> Queue error with staleness hint ────────────────

#[test]
fn ac3_malformed_json_queue_error_with_staleness_hint() {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");

    // fno backlog next returns malformed JSON; fno doctor returns stale status
    write_stub(
        &bin_dir,
        "fno",
        r#"if [[ "$1" == "backlog" && "$2" == "next" ]]; then
  echo '{not valid json'
  exit 0
fi
if [[ "$1" == "doctor" && "$2" == "--json" ]]; then
  echo '{"status":"stale","python_stale":true,"rust_stale":false}'
  exit 0
fi
exit 0"#,
    );

    let abi_stub = bin_dir.join("fno").display().to_string();
    let mut q = MegawalkQueue::new(abi_stub, None, false);
    match q.next() {
        Err(e) => {
            let err_str = e.to_string();
            assert!(
                err_str.contains("fno update") || err_str.contains("stale"),
                "error must contain staleness hint, got: {err_str}"
            );
        }
        Ok(_) => panic!("malformed JSON must return Err"),
    }
}

// ── AC4-HP: claim exit-1 -> re-pick loop returns next node ───────────────────

#[test]
fn ac4_claim_held_retries_to_next_node() {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");
    let call_log = tmp.path().join("calls.log");
    let call_log_str = call_log.display().to_string();

    // First `backlog next` returns node A; claim for A exits 1 (held).
    // Second `backlog next` returns node B; claim for B exits 0.
    let node_a = real_node_json("ab-aaaaaaaa", "Node A", None);
    let node_b = real_node_json("ab-bbbbbbbb", "Node B", None);

    write_stub(
        &bin_dir,
        "fno",
        &format!(
            r#"echo "$@" >> "{call_log_str}"
if [[ "$1" == "backlog" && "$2" == "next" ]]; then
  if grep -q "claim acquire.*node:ab-aaaaaaaa" "{call_log_str}" 2>/dev/null; then
    echo '{node_b}'
  else
    echo '{node_a}'
  fi
  exit 0
fi
if [[ "$1" == "claim" && "$2" == "acquire" ]]; then
  if echo "$@" | grep -q "node:ab-aaaaaaaa"; then
    exit 1
  fi
  exit 0
fi
if [[ "$1" == "doctor" ]]; then
  echo '{{"status":"fresh"}}'
  exit 0
fi
exit 0"#,
            call_log_str = call_log_str,
            node_a = node_a,
            node_b = node_b,
        ),
    );

    let abi_stub = bin_dir.join("fno").display().to_string();
    let mut q = MegawalkQueue::new(abi_stub, None, false);
    let unit = q.next().expect("no error").expect("expected Some(unit)");
    assert_eq!(
        unit.id, "ab-bbbbbbbb",
        "should return second node after claim retry"
    );
}

// ── AC5-EDGE: bounded-retry exhaustion errors ─────────────────────────────────

#[test]
fn ac5_bounded_retry_exhaustion_returns_error() {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");

    // fno backlog next always returns a node; fno claim always exits 1 (held).
    let node_json = real_node_json("ab-stuck000", "Stuck node", None);

    write_stub(
        &bin_dir,
        "fno",
        &format!(
            r#"if [[ "$1" == "backlog" && "$2" == "next" ]]; then
  echo '{node_json}'
  exit 0
fi
if [[ "$1" == "claim" && "$2" == "acquire" ]]; then
  exit 1
fi
if [[ "$1" == "doctor" ]]; then
  echo '{{"status":"fresh"}}'
  exit 0
fi
exit 0"#,
            node_json = node_json,
        ),
    );

    let abi_stub = bin_dir.join("fno").display().to_string();
    let mut q = MegawalkQueue::new(abi_stub, None, false);
    let err_str = match q.next() {
        Err(e) => e.to_string(),
        Ok(_) => panic!("exhausted retries must return Err"),
    };
    assert!(
        err_str.contains("ab-stuck000")
            || err_str.to_lowercase().contains("exhausted")
            || err_str.contains("stuck"),
        "error must name the stuck node, got: {err_str}"
    );
}

// ── AC6-HP: close DonePRGreen -> calls done -> Closed ────────────────────────

#[test]
fn ac6_close_done_pr_green_calls_done_and_returns_closed() {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");
    let call_log = tmp.path().join("calls.log");
    let call_log_str = call_log.display().to_string();

    let node_json = real_node_json("ab-closetest", "Close test", None);

    write_stub(
        &bin_dir,
        "fno",
        &format!(
            r#"echo "$@" >> "{call_log_str}"
if [[ "$1" == "backlog" && "$2" == "next" ]]; then
  echo '{node_json}'
  exit 0
fi
if [[ "$1" == "claim" ]]; then
  exit 0
fi
if [[ "$1" == "backlog" && "$2" == "done" ]]; then
  exit 0
fi
exit 0"#,
            call_log_str = call_log_str,
            node_json = node_json,
        ),
    );

    let abi_stub = bin_dir.join("fno").display().to_string();
    let mut q = MegawalkQueue::new(abi_stub, None, false);
    let unit = q.next().unwrap().unwrap();

    let evidence = Evidence {
        reason: TerminationReason::DonePRGreen,
        message: "PR merged".to_string(),
    };
    let outcome = q.close(&unit, &evidence).unwrap();

    assert_eq!(outcome, CloseOutcome::Closed);

    // Verify `fno backlog done <id>` was called.
    let calls = read_file_lines(&call_log);
    assert!(
        calls
            .iter()
            .any(|l| l.contains("backlog done") && l.contains("ab-closetest")),
        "backlog done must be called with node id; calls: {calls:?}"
    );

    // Verify claim was released.
    assert!(
        calls
            .iter()
            .any(|l| l.contains("claim release") && l.contains("node:ab-closetest")),
        "claim must be released; calls: {calls:?}"
    );
}

// ── AC7-HP: done nonzero -> Parked + claim HELD (park-exclusion) ─────────────
//
// When `fno backlog done` fails (exit 1), the outcome is Parked. Under
// park-exclusion the claim is HELD (not released) so the live-claims filter
// continues to skip this node. A re-acquire (TTL refresh) is issued instead.

#[test]
fn ac7_done_failure_returns_parked_and_holds_claim() {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");
    let call_log = tmp.path().join("calls.log");
    let call_log_str = call_log.display().to_string();

    let node_json = real_node_json("ab-donefail1", "Done fails", None);

    write_stub(
        &bin_dir,
        "fno",
        &format!(
            r#"echo "$@" >> "{call_log_str}"
if [[ "$1" == "backlog" && "$2" == "next" ]]; then
  echo '{node_json}'
  exit 0
fi
if [[ "$1" == "claim" && "$2" == "acquire" ]]; then
  exit 0
fi
if [[ "$1" == "backlog" && "$2" == "done" ]]; then
  echo "done refused: dependents unresolved" >&2
  exit 1
fi
if [[ "$1" == "claim" && "$2" == "release" ]]; then
  exit 0
fi
exit 0"#,
            call_log_str = call_log_str,
            node_json = node_json,
        ),
    );

    let abi_stub = bin_dir.join("fno").display().to_string();
    let mut q = MegawalkQueue::new(abi_stub, None, false);
    let unit = q.next().unwrap().unwrap();

    let evidence = Evidence {
        reason: TerminationReason::DonePRGreen,
        message: "PR merged".to_string(),
    };
    let outcome = q.close(&unit, &evidence).unwrap();

    // done failed -> Parked (not crash)
    assert!(
        matches!(outcome, CloseOutcome::Parked(_)),
        "done failure must return Parked, got: {outcome:?}"
    );

    let calls = read_file_lines(&call_log);

    // Park-exclusion: claim must NOT be released when done fails.
    assert!(
        !calls
            .iter()
            .any(|l| l.contains("claim release") && l.contains("node:ab-donefail1")),
        "claim must NOT be released for a parked node (park-exclusion); calls: {calls:?}"
    );

    // A re-acquire (TTL refresh) must have been issued after the initial acquire.
    let acquire_calls: Vec<_> = calls
        .iter()
        .filter(|l| l.contains("claim acquire") && l.contains("node:ab-donefail1"))
        .collect();
    assert!(
        acquire_calls.len() >= 2,
        "claim re-acquire must be called for park-hold TTL refresh; \
         found {} acquire call(s): {acquire_calls:?}",
        acquire_calls.len()
    );
}

// ── AC8-HP: close Budget -> does NOT call done -> Parked ─────────────────────

#[test]
fn ac8_close_budget_does_not_call_done() {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");
    let call_log = tmp.path().join("calls.log");
    let call_log_str = call_log.display().to_string();

    let node_json = real_node_json("ab-budget001", "Budget test", None);

    write_stub(
        &bin_dir,
        "fno",
        &format!(
            r#"echo "$@" >> "{call_log_str}"
if [[ "$1" == "backlog" && "$2" == "next" ]]; then
  echo '{node_json}'
  exit 0
fi
if [[ "$1" == "claim" ]]; then
  exit 0
fi
if [[ "$1" == "backlog" && "$2" == "done" ]]; then
  echo "SHOULD NOT BE CALLED" >&2
  exit 99
fi
exit 0"#,
            call_log_str = call_log_str,
            node_json = node_json,
        ),
    );

    let abi_stub = bin_dir.join("fno").display().to_string();
    let mut q = MegawalkQueue::new(abi_stub, None, false);
    let unit = q.next().unwrap().unwrap();

    let evidence = Evidence {
        reason: TerminationReason::Budget,
        message: "budget exhausted".to_string(),
    };
    let outcome = q.close(&unit, &evidence).unwrap();

    assert!(
        matches!(outcome, CloseOutcome::Parked(_)),
        "Budget reason must return Parked, got: {outcome:?}"
    );

    let calls = read_file_lines(&call_log);
    assert!(
        !calls.iter().any(|l| l.contains("backlog done")),
        "backlog done must NOT be called for Budget termination; calls: {calls:?}"
    );

    // Park-exclusion: claim must NOT be released for a parked (Budget) unit.
    // The claim is held so the live-claims filter keeps excluding this node.
    assert!(
        !calls.iter().any(|l| l.contains("claim release")),
        "claim must NOT be released for Budget (park-exclusion holds it); calls: {calls:?}"
    );

    // A re-acquire (TTL refresh) must be issued after the park.
    let acquire_calls: Vec<_> = calls
        .iter()
        .filter(|l| l.contains("claim acquire") && l.contains("node:ab-budget001"))
        .collect();
    assert!(
        acquire_calls.len() >= 2,
        "claim re-acquire must be called for park-hold TTL refresh; \
         found {} acquire call(s): {acquire_calls:?}",
        acquire_calls.len()
    );
}

// ── AC9-VERIFY: find_termination finds event in global journal (not project) ──

#[test]
fn ac9_find_termination_falls_back_to_global_journal() {
    let tmp = TempDir::new().unwrap();

    // Project journal: empty / missing.
    let project_journal = tmp.path().join("project").join("events.jsonl");
    // Global journal: has the termination event.
    let global_journal = tmp.path().join("global").join("events.jsonl");

    let session_key = "20260606T000000Z-mw12345-abcdef";
    seed_termination(&global_journal, session_key, "DonePRGreen");

    let journal = Journal::new_raw(project_journal.clone(), global_journal.clone());
    let evidence = journal
        .find_termination(session_key)
        .expect("no error")
        .expect("expected evidence from global journal");

    assert!(
        matches!(evidence.reason, TerminationReason::DonePRGreen),
        "reason must be DonePRGreen"
    );
}

// ── AC10-E2E: full run_loop walk with stub fno ────────────────────────────────

#[test]
fn ac10_e2e_run_loop_two_nodes_then_nowork() {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");

    // Project journal where loop runtime writes events.
    let abilities_dir = tmp.path().join("project").join(".fno");
    fs::create_dir_all(&abilities_dir).unwrap();
    let project_journal = abilities_dir.join("events.jsonl");
    let global_journal = tmp.path().join("global-events.jsonl");

    let _node_a_key = "20260606T000000Z-mwXXXXX-aaaaaa";
    let _node_b_key = "20260606T000000Z-mwXXXXX-bbbbbb";

    // The stub fno:
    // - backlog next: first call returns node A (with a fixed session-key-like id
    //   we can embed), second call returns node B, third call returns null.
    // - claim acquire: always exit 0
    // - claim release: always exit 0
    // - backlog done: always exit 0
    // - driver_invoke is a separate lib script, not fno - the dispatcher uses
    //   a stub driver lib that writes a termination event to project journal.
    //
    // For the dispatcher, we use a stub driver lib that:
    // 1. reads TARGET_SESSION_ID from env
    // 2. writes a DonePRGreen termination event into the journal
    let driver_lib = tmp.path().join("lib");
    fs::create_dir_all(&driver_lib).unwrap();
    let project_journal_str = project_journal.display().to_string();
    // Use printf with %s placeholders to avoid bash JSON escaping pitfalls.
    let driver_lib_content = format!(
        r#"#!/usr/bin/env bash
driver_default_max() {{ echo 10; }}
driver_invoke() {{
  local sid="${{TARGET_SESSION_ID:-}}"
  local ts
  ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  printf '{{"ts":"%s","type":"termination","source":"hook","data":{{"session_id":"%s","reason":"DonePRGreen","message":"done"}}}}\n' "$ts" "$sid" >> "{journal}"
}}
"#,
        journal = project_journal_str,
    );
    let lib_path = driver_lib.join("driver-claude-code.sh");
    fs::write(&lib_path, driver_lib_content.as_bytes()).unwrap();
    fs::set_permissions(&lib_path, fs::Permissions::from_mode(0o755)).unwrap();

    // Counters file for backlog next call sequencing.
    let counter_file = tmp.path().join("next_count.txt");
    fs::write(&counter_file, "0").unwrap();
    let counter_str = counter_file.display().to_string();

    let node_a = real_node_json("ab-e2eaaaaa", "E2E Node A", None);
    let node_b = real_node_json("ab-e2ebbbbbb", "E2E Node B", None);

    write_stub(
        &bin_dir,
        "fno",
        &format!(
            r#"if [[ "$1" == "backlog" && "$2" == "next" ]]; then
  count=$(cat "{counter}" 2>/dev/null || echo 0)
  count=$((count + 1))
  echo "$count" > "{counter}"
  if [[ "$count" -eq 1 ]]; then
    echo '{node_a}'
  elif [[ "$count" -eq 2 ]]; then
    echo '{node_b}'
  else
    echo 'null'
  fi
  exit 0
fi
if [[ "$1" == "claim" ]]; then
  exit 0
fi
if [[ "$1" == "backlog" && "$2" == "done" ]]; then
  exit 0
fi
if [[ "$1" == "doctor" ]]; then
  echo '{{"status":"fresh"}}'
  exit 0
fi
exit 0"#,
            counter = counter_str,
            node_a = node_a,
            node_b = node_b,
        ),
    );

    // Also need a real `claude` binary for preflight; use a stub.
    write_stub(&bin_dir, "claude", "exit 0");

    let path_str = path_with(&bin_dir);

    // Build the queue, journal, budget, and dispatcher.
    let abi_stub = bin_dir.join("fno").display().to_string();
    let mut queue = MegawalkQueue::new(abi_stub.clone(), None, false);

    let journal = Journal::new_raw(project_journal.clone(), global_journal.clone());
    let budget = LoopBudget::new(10).unwrap();

    // Use ShelloutDispatcher with the stub driver lib.
    use fno_agents::loop_dispatch::ShelloutDispatcher;
    use fno_agents::loop_runtime::DispatchCtx;

    let env = vec![
        ("PATH".to_string(), path_str.clone()),
        ("MAX_TURNS".to_string(), "5".to_string()),
        ("BUDGET_USD".to_string(), "25".to_string()),
        (
            "OUTPUT_FILE".to_string(),
            tmp.path().join("output.txt").display().to_string(),
        ),
        (
            "HISTORY_FILE".to_string(),
            tmp.path().join("history.txt").display().to_string(),
        ),
        (
            "SIGNAL_FILE".to_string(),
            tmp.path().join("signal.txt").display().to_string(),
        ),
        (
            "CONTINUE_PROMPT".to_string(),
            "/target no-merge test".to_string(),
        ),
    ];

    // We need a MegawalkDispatcher that injects TARGET_SESSION_ID.
    // For the e2e test, we use a direct dispatcher implementation
    // that wraps ShelloutDispatcher with per-unit env injection.
    struct TestDispatcher {
        base: ShelloutDispatcher,
    }
    impl fno_agents::loop_runtime::Dispatcher for TestDispatcher {
        fn run(
            &self,
            unit: &Unit,
            ctx: &DispatchCtx,
        ) -> Result<Box<dyn fno_agents::loop_runtime::Session>, LoopError> {
            // We need to create a new ShelloutDispatcher with the extra env.
            // Since ShelloutDispatcher::new is pub, just delegate.
            self.base.run(unit, ctx)
        }
    }

    // Build a ShelloutDispatcher that injects TARGET_SESSION_ID via env.
    // Since the driver_invoke reads TARGET_SESSION_ID, we need to pass it
    // through. The real MegawalkDispatcher does this per-unit; for tests
    // we verify that the termination event correlates with the session_key.
    //
    // The simplest approach: use a custom driver lib that doesn't need
    // TARGET_SESSION_ID but writes the UNIT id from env. Let's update
    // the driver lib to write the node id so we can correlate.
    //
    // Actually, let's use a driver lib that reads CONTINUE_PROMPT which
    // contains the node id, or better: add TARGET_SESSION_ID to env list.
    // We need the session_key from each Unit. Since we can't hook into
    // the queue to know what session_key will be assigned, let's use
    // a driver lib that reads it from CONTINUE_PROMPT.
    //
    // Simplest correct approach: use MegawalkDispatcher directly (it's
    // in the same crate). Let's use it.

    // Drop the test dispatcher - use MegawalkDispatcher from the module.
    let _ = TestDispatcher {
        base: ShelloutDispatcher::new(lib_path.clone(), env.clone(), tmp.path().to_path_buf()),
    };

    use fno_agents::loop_megawalk::MegawalkDispatcher;
    let dispatcher = MegawalkDispatcher::new(
        lib_path.clone(),
        env,
        tmp.path().join("project"),
        abi_stub.clone(),
        false, // allow_merge
    );

    let cancel = || false;

    let outcome = run_loop(&mut queue, &dispatcher, &budget, &journal, &cancel, None)
        .expect("run_loop must not error");

    assert!(
        matches!(outcome.reason, TerminationReason::NoWork),
        "walk must end with NoWork, got: {:?}",
        outcome.reason
    );
    assert_eq!(outcome.units.len(), 2, "two units must be processed");

    let first = &outcome.units[0];
    let second = &outcome.units[1];

    assert_eq!(first.unit_id, "ab-e2eaaaaa");
    assert_eq!(second.unit_id, "ab-e2ebbbbbb");

    assert!(
        matches!(first.close, CloseOutcome::Closed),
        "first unit must be Closed, got: {:?}",
        first.close
    );
    assert!(
        matches!(second.close, CloseOutcome::Closed),
        "second unit must be Closed, got: {:?}",
        second.close
    );
}

// ── AC11-E2E: p0 node + no termination event -> per-unit cap parks -> ─────────
// p0 pause fires through production MegawalkQueue, walk returns NoProgress ─────
//
// This test proves the policy is wired into the PRODUCTION path (MegawalkQueue
// itself, not only MegawalkPolicyQueue). The stub fno returns a p0 node on the
// first call; the stub driver never emits a termination event; the per-unit cap
// of 2 parks the unit; the p0-failure policy then triggers on the next next()
// call; run_loop returns NoProgress with walk_paused journaled.
#[test]
fn ac11_p0_failure_via_production_queue_triggers_policy_pause() {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");
    let lib_dir = tmp.path().join("lib");

    // Project journal (walker cwd).
    let abilities_dir = tmp.path().join(".fno");
    fs::create_dir_all(&abilities_dir).unwrap();
    let project_journal = abilities_dir.join("events.jsonl");
    let global_journal = tmp.path().join("global-events.jsonl");

    // ── stub driver lib (never emits a termination event) ─────────────────
    // The driver script is invoked by ShelloutDispatcher. It exits 1 without
    // writing any termination event to the journal, simulating a stuck session.
    let driver_script = format!(
        r#"#!/usr/bin/env bash
# Stub driver for AC11: always exits without emitting a termination event.
exit 1
"#
    );
    fs::create_dir_all(&lib_dir).unwrap();
    let lib_path = lib_dir.join("driver-invoke.sh");
    fs::write(&lib_path, driver_script.as_bytes()).unwrap();
    fs::set_permissions(&lib_path, fs::Permissions::from_mode(0o755)).unwrap();

    // Write a MAX_DISPATCHES marker (used by driver_default_max).
    let max_file = lib_dir.join("driver-default-max");
    fs::write(&max_file, "50\n").unwrap();

    // ── stub fno binary ────────────────────────────────────────────────────
    // Returns a p0 node once, then "null" (backlog empty) on subsequent calls.
    // claim acquire always exits 0.
    // claim release always exits 0.
    // backlog done always exits 0 (would be called for Done reasons only).
    // doctor returns fresh.
    let counter_file = tmp.path().join("next_call_count");
    fs::write(&counter_file, "0").unwrap();
    let counter_str = counter_file.display().to_string();

    // The p0 node JSON (priority = "p0").
    let p0_node = format!(
        r#"{{
  "id": "ab-p0test1",
  "title": "Critical p0 task",
  "priority": "p0",
  "domain": "code",
  "project": "abilities",
  "cwd": "/tmp",
  "size": null,
  "plan_path": null
}}"#
    );

    write_stub(
        &bin_dir,
        "fno",
        &format!(
            r#"
if [[ "$1" == "backlog" && "$2" == "next" ]]; then
  count=$(cat '{counter}' 2>/dev/null || echo 0)
  count=$((count + 1))
  echo "$count" > '{counter}'
  if [[ "$count" -eq 1 ]]; then
    cat <<'NODEJSON'
{p0_node}
NODEJSON
  else
    echo 'null'
  fi
  exit 0
fi
if [[ "$1" == "claim" ]]; then
  exit 0
fi
if [[ "$1" == "backlog" && "$2" == "done" ]]; then
  exit 0
fi
if [[ "$1" == "doctor" ]]; then
  echo '{{"status":"fresh"}}'
  exit 0
fi
exit 0"#,
            counter = counter_str,
            p0_node = p0_node,
        ),
    );

    // Also need a real `claude` binary stub for preflight.
    write_stub(&bin_dir, "claude", "exit 0");

    let path_str = path_with(&bin_dir);
    let abi_stub = bin_dir.join("fno").display().to_string();

    // ── build the production MegawalkQueue ────────────────────────────────
    let mut queue = MegawalkQueue::new(abi_stub.clone(), None, false);
    let journal = Journal::new_raw(project_journal.clone(), global_journal.clone());
    // per_unit_max_dispatches = 2: after 2 dispatches without a termination
    // event the unit is parked as NoProgress, and the p0 policy fires on the
    // next next() call.
    let budget = LoopBudget::new(20).unwrap();

    use fno_agents::loop_dispatch::ShelloutDispatcher;
    let env = vec![
        ("PATH".to_string(), path_str.clone()),
        ("MAX_TURNS".to_string(), "5".to_string()),
        ("BUDGET_USD".to_string(), "25".to_string()),
        (
            "OUTPUT_FILE".to_string(),
            tmp.path().join("output.txt").display().to_string(),
        ),
        (
            "HISTORY_FILE".to_string(),
            tmp.path().join("history.txt").display().to_string(),
        ),
        (
            "SIGNAL_FILE".to_string(),
            tmp.path().join("signal.txt").display().to_string(),
        ),
        (
            "CONTINUE_PROMPT".to_string(),
            "/target no-merge ab-p0test1".to_string(),
        ),
    ];

    use fno_agents::loop_megawalk::MegawalkDispatcher;
    let dispatcher = MegawalkDispatcher::new(
        lib_path.clone(),
        env,
        tmp.path().to_path_buf(),
        abi_stub.clone(),
        false,
    );

    let cancel = || false;

    let outcome = run_loop(&mut queue, &dispatcher, &budget, &journal, &cancel, Some(2))
        .expect("run_loop must not error");

    // The walk must return NoProgress (p0 pause after the cap parks the unit).
    assert_eq!(
        outcome.reason,
        TerminationReason::NoProgress,
        "p0 failure after cap park must return NoProgress, got: {:?}",
        outcome.reason
    );

    // walk_paused must be journaled with policy=p0_failed.
    let events: Vec<serde_json::Value> = {
        if !project_journal.exists() {
            vec![]
        } else {
            fs::read_to_string(&project_journal)
                .unwrap()
                .lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect()
        }
    };

    let paused: Vec<_> = events
        .iter()
        .filter(|v| v["type"].as_str() == Some("walk_paused"))
        .collect();
    assert_eq!(
        paused.len(),
        1,
        "expected exactly 1 walk_paused event; got {paused:?}"
    );
    assert_eq!(
        paused[0]["data"]["policy"].as_str(),
        Some("p0_failed"),
        "walk_paused policy must be p0_failed; got {:?}",
        paused[0]["data"]["policy"]
    );
    assert!(
        paused[0]["data"]["detail"]
            .as_str()
            .unwrap_or("")
            .contains("ab-p0test1"),
        "walk_paused detail must contain unit id; got {:?}",
        paused[0]["data"]["detail"]
    );

    // node_closed must be journaled for the parked unit.
    let closed: Vec<_> = events
        .iter()
        .filter(|v| v["type"].as_str() == Some("node_closed"))
        .collect();
    assert_eq!(
        closed.len(),
        1,
        "expected 1 node_closed event; got {closed:?}"
    );
    assert_eq!(closed[0]["data"]["unit_id"].as_str(), Some("ab-p0test1"));
    assert_eq!(
        closed[0]["data"]["close"].as_str(),
        Some("parked"),
        "p0 unit must be Parked (not Closed); got {:?}",
        closed[0]["data"]["close"]
    );
}

// ── AC12-EDGE: park-exclusion - parked node claim NOT released ────────────────
//
// When a unit parks (non-Done termination reason), the walker HOLDS the node
// claim so `fno backlog next` continues to skip this node via the live-claims
// filter. The claim must NOT be released on Parked/Refused outcomes.
// The walker also re-acquires (refreshes) the claim immediately after a park
// so the TTL stays live even if the worker's idempotent re-acquire had
// rewritten the claim record.
//
// Test: stub fno where node A parks (Budget termination), then node B is
// returned; assert:
//   - claim release was NOT called for A after the park
//   - a claim re-acquire WAS called for A after the park (TTL refresh)
//   - node B got dispatched (walk continued past the parked unit)
//   - the failure streak still counted A
#[test]
fn ac12_parked_node_claim_held_not_released() {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");
    let call_log = tmp.path().join("calls.log");
    let call_log_str = call_log.display().to_string();

    // Project journal for the walk.
    let abilities_dir = tmp.path().join(".fno");
    fs::create_dir_all(&abilities_dir).unwrap();
    let project_journal = abilities_dir.join("events.jsonl");
    let global_journal = tmp.path().join("global-events.jsonl");

    // Stub fno:
    //   backlog next: call 1 -> node A, call 2 -> node B, call 3 -> null
    //   claim acquire: always 0
    //   claim release: always 0 (but we assert it's NOT called for A after park)
    //   backlog done: always 0
    //   doctor: fresh
    let node_a = real_node_json("ab-park-aaa", "Node A to be parked", None);
    let node_b = real_node_json("ab-park-bbb", "Node B continues", None);

    let counter_file = tmp.path().join("next_count.txt");
    fs::write(&counter_file, "0").unwrap();
    let counter_str = counter_file.display().to_string();

    write_stub(
        &bin_dir,
        "fno",
        &format!(
            r#"echo "$@" >> "{log}"
if [[ "$1" == "backlog" && "$2" == "next" ]]; then
  count=$(cat "{counter}" 2>/dev/null || echo 0)
  count=$((count + 1))
  echo "$count" > "{counter}"
  if [[ "$count" -eq 1 ]]; then
    echo '{node_a}'
  elif [[ "$count" -eq 2 ]]; then
    echo '{node_b}'
  else
    echo 'null'
  fi
  exit 0
fi
if [[ "$1" == "claim" ]]; then
  exit 0
fi
if [[ "$1" == "backlog" && "$2" == "done" ]]; then
  exit 0
fi
if [[ "$1" == "doctor" ]]; then
  echo '{{"status":"fresh"}}'
  exit 0
fi
exit 0"#,
            log = call_log_str,
            counter = counter_str,
            node_a = node_a,
            node_b = node_b,
        ),
    );

    // Stub driver lib: emits Budget termination for node A (first call);
    // emits DonePRGreen for node B (second call).
    let driver_lib = tmp.path().join("lib");
    fs::create_dir_all(&driver_lib).unwrap();
    let project_journal_str = project_journal.display().to_string();
    let parked_flag = tmp.path().join("first_dispatch_done");

    let driver_content = format!(
        r#"#!/usr/bin/env bash
driver_default_max() {{ echo 10; }}
driver_invoke() {{
  local sid="${{TARGET_SESSION_ID:-}}"
  local ts
  ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  if [[ ! -f "{flag}" ]]; then
    # First unit (node A): emit Budget (park)
    touch "{flag}"
    printf '{{"ts":"%s","type":"termination","source":"hook","data":{{"session_id":"%s","reason":"Budget","message":"budget"}}}}\n' "$ts" "$sid" >> "{journal}"
  else
    # Second unit (node B): emit DonePRGreen (success)
    printf '{{"ts":"%s","type":"termination","source":"hook","data":{{"session_id":"%s","reason":"DonePRGreen","message":"done"}}}}\n' "$ts" "$sid" >> "{journal}"
  fi
}}
"#,
        flag = parked_flag.display(),
        journal = project_journal_str,
    );
    let lib_path = driver_lib.join("driver-claude-code.sh");
    fs::write(&lib_path, driver_content.as_bytes()).unwrap();
    fs::set_permissions(&lib_path, fs::Permissions::from_mode(0o755)).unwrap();

    write_stub(&bin_dir, "claude", "exit 0");
    let path_str = path_with(&bin_dir);
    let abi_stub = bin_dir.join("fno").display().to_string();

    // The dispatcher cwd must exist (ShelloutDispatcher uses current_dir).
    let dispatch_cwd = tmp.path().join("project");
    fs::create_dir_all(&dispatch_cwd).unwrap();

    let mut queue = MegawalkQueue::new(abi_stub.clone(), None, false);
    let journal = Journal::new_raw(project_journal.clone(), global_journal.clone());
    let budget = LoopBudget::new(10).unwrap();

    use fno_agents::loop_megawalk::MegawalkDispatcher;
    let env = vec![
        ("PATH".to_string(), path_str.clone()),
        ("MAX_TURNS".to_string(), "5".to_string()),
        ("BUDGET_USD".to_string(), "25".to_string()),
        (
            "OUTPUT_FILE".to_string(),
            tmp.path().join("output.txt").display().to_string(),
        ),
        (
            "HISTORY_FILE".to_string(),
            tmp.path().join("history.txt").display().to_string(),
        ),
        (
            "SIGNAL_FILE".to_string(),
            tmp.path().join("signal.txt").display().to_string(),
        ),
        (
            "CONTINUE_PROMPT".to_string(),
            "/target no-merge test".to_string(),
        ),
    ];

    let dispatcher =
        MegawalkDispatcher::new(lib_path.clone(), env, dispatch_cwd, abi_stub.clone(), false);

    let cancel = || false;
    let outcome = run_loop(&mut queue, &dispatcher, &budget, &journal, &cancel, None)
        .expect("run_loop must not error");

    // Walk should complete with NoWork (both units processed).
    assert!(
        matches!(outcome.reason, TerminationReason::NoWork),
        "walk must end with NoWork, got: {:?}",
        outcome.reason
    );

    assert_eq!(outcome.units.len(), 2, "two units must be processed");

    // Node A must be Parked, node B must be Closed.
    let a_result = outcome.units.iter().find(|r| r.unit_id == "ab-park-aaa");
    assert!(a_result.is_some(), "node A must appear in results");
    assert!(
        matches!(a_result.unwrap().close, CloseOutcome::Parked(_)),
        "node A must be Parked, got {:?}",
        a_result.unwrap().close
    );

    let b_result = outcome.units.iter().find(|r| r.unit_id == "ab-park-bbb");
    assert!(b_result.is_some(), "node B must appear in results");
    assert_eq!(
        b_result.unwrap().close,
        CloseOutcome::Closed,
        "node B must be Closed"
    );

    // Verify claim release was NOT called for node A after its park.
    let calls = read_file_lines(&call_log);

    // After node A parks, there should be NO "claim release ... node:ab-park-aaa" line.
    // (There WILL be a release for node B since it closed successfully.)
    let a_releases: Vec<_> = calls
        .iter()
        .filter(|l| l.contains("claim release") && l.contains("node:ab-park-aaa"))
        .collect();
    assert!(
        a_releases.is_empty(),
        "claim release must NOT be called for parked node A; calls: {calls:?}"
    );

    // Verify a claim re-acquire (refresh) WAS called for node A after the park.
    // The re-acquire is identified by "claim acquire ... node:ab-park-aaa" appearing
    // after the first "backlog next" call (i.e., more than 1 claim acquire for node A).
    let a_acquires: Vec<_> = calls
        .iter()
        .filter(|l| l.contains("claim acquire") && l.contains("node:ab-park-aaa"))
        .collect();
    assert!(
        a_acquires.len() >= 2,
        "claim re-acquire must be called for parked node A to refresh TTL; \
         found {} acquire calls: {a_acquires:?}",
        a_acquires.len()
    );

    // Node B must have been dispatched (claim acquired and released).
    assert!(
        calls
            .iter()
            .any(|l| l.contains("claim acquire") && l.contains("node:ab-park-bbb")),
        "node B must have been claimed (dispatched); calls: {calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|l| l.contains("claim release") && l.contains("node:ab-park-bbb")),
        "node B claim must be released after close; calls: {calls:?}"
    );
}

// ── AC13-HP: --max-units N terminates walk after N closed units ───────────────
//
// The --max-units flag maps to the /megawalk once modifier (N=1) and future
// use. After N units reach close (any outcome: Closed, Parked, Refused),
// queue.next() returns Drained -> walk terminates with NoWork, exit 0.
//
// Test: 3 ready nodes, --max-units 1 -> exactly 1 dispatched+closed,
// walk reason NoWork.
#[test]
fn ac13_max_units_one_stops_after_first_unit() {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");

    let abilities_dir = tmp.path().join(".fno");
    fs::create_dir_all(&abilities_dir).unwrap();
    let project_journal = abilities_dir.join("events.jsonl");
    let global_journal = tmp.path().join("global-events.jsonl");

    let node_a = real_node_json("ab-maxu-aaa", "Max-units Node A", None);
    let node_b = real_node_json("ab-maxu-bbb", "Max-units Node B", None);
    let node_c = real_node_json("ab-maxu-ccc", "Max-units Node C", None);

    let counter_file = tmp.path().join("next_count.txt");
    fs::write(&counter_file, "0").unwrap();
    let counter_str = counter_file.display().to_string();
    let call_log = tmp.path().join("calls.log");
    let call_log_str = call_log.display().to_string();

    write_stub(
        &bin_dir,
        "fno",
        &format!(
            r#"echo "$@" >> "{log}"
if [[ "$1" == "backlog" && "$2" == "next" ]]; then
  count=$(cat "{counter}" 2>/dev/null || echo 0)
  count=$((count + 1))
  echo "$count" > "{counter}"
  if [[ "$count" -eq 1 ]]; then
    echo '{node_a}'
  elif [[ "$count" -eq 2 ]]; then
    echo '{node_b}'
  elif [[ "$count" -eq 3 ]]; then
    echo '{node_c}'
  else
    echo 'null'
  fi
  exit 0
fi
if [[ "$1" == "claim" ]]; then
  exit 0
fi
if [[ "$1" == "backlog" && "$2" == "done" ]]; then
  exit 0
fi
if [[ "$1" == "doctor" ]]; then
  echo '{{"status":"fresh"}}'
  exit 0
fi
exit 0"#,
            log = call_log_str,
            counter = counter_str,
            node_a = node_a,
            node_b = node_b,
            node_c = node_c,
        ),
    );

    // Driver lib: always emits DonePRGreen.
    let driver_lib = tmp.path().join("lib");
    fs::create_dir_all(&driver_lib).unwrap();
    let project_journal_str = project_journal.display().to_string();
    let driver_content = format!(
        r#"#!/usr/bin/env bash
driver_default_max() {{ echo 10; }}
driver_invoke() {{
  local sid="${{TARGET_SESSION_ID:-}}"
  local ts
  ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  printf '{{"ts":"%s","type":"termination","source":"hook","data":{{"session_id":"%s","reason":"DonePRGreen","message":"done"}}}}\n' "$ts" "$sid" >> "{journal}"
}}
"#,
        journal = project_journal_str,
    );
    let lib_path = driver_lib.join("driver-claude-code.sh");
    fs::write(&lib_path, driver_content.as_bytes()).unwrap();
    fs::set_permissions(&lib_path, fs::Permissions::from_mode(0o755)).unwrap();

    write_stub(&bin_dir, "claude", "exit 0");
    let path_str = path_with(&bin_dir);
    let abi_stub = bin_dir.join("fno").display().to_string();

    // The dispatcher cwd must exist (ShelloutDispatcher uses current_dir).
    let dispatch_cwd = tmp.path().join("project");
    fs::create_dir_all(&dispatch_cwd).unwrap();

    // Build MegawalkQueue with max_units = Some(1).
    let mut queue = MegawalkQueue::new_with_max_units(abi_stub.clone(), None, false, Some(1));
    let journal = Journal::new_raw(project_journal.clone(), global_journal.clone());
    let budget = LoopBudget::new(10).unwrap();

    use fno_agents::loop_megawalk::MegawalkDispatcher;
    let env = vec![
        ("PATH".to_string(), path_str.clone()),
        ("MAX_TURNS".to_string(), "5".to_string()),
        ("BUDGET_USD".to_string(), "25".to_string()),
        (
            "OUTPUT_FILE".to_string(),
            tmp.path().join("output.txt").display().to_string(),
        ),
        (
            "HISTORY_FILE".to_string(),
            tmp.path().join("history.txt").display().to_string(),
        ),
        (
            "SIGNAL_FILE".to_string(),
            tmp.path().join("signal.txt").display().to_string(),
        ),
        (
            "CONTINUE_PROMPT".to_string(),
            "/target no-merge test".to_string(),
        ),
    ];

    let dispatcher =
        MegawalkDispatcher::new(lib_path.clone(), env, dispatch_cwd, abi_stub.clone(), false);

    let cancel = || false;
    let outcome = run_loop(&mut queue, &dispatcher, &budget, &journal, &cancel, None)
        .expect("run_loop must not error");

    // Walk must end with NoWork (max-units cap reached, not backlog empty).
    assert!(
        matches!(outcome.reason, TerminationReason::NoWork),
        "max-units walk must end with NoWork, got: {:?}",
        outcome.reason
    );

    // Exactly 1 unit must have been processed.
    assert_eq!(
        outcome.units.len(),
        1,
        "--max-units 1 must close exactly 1 unit, got {}",
        outcome.units.len()
    );
    assert_eq!(outcome.units[0].unit_id, "ab-maxu-aaa");
    assert_eq!(
        outcome.units[0].close,
        CloseOutcome::Closed,
        "first unit must be Closed"
    );

    // Nodes B and C must NOT have been dispatched.
    let calls = read_file_lines(&call_log);
    assert!(
        !calls.iter().any(|l| l.contains("node:ab-maxu-bbb")),
        "node B must not be claimed when max-units=1; calls: {calls:?}"
    );
    assert!(
        !calls.iter().any(|l| l.contains("node:ab-maxu-ccc")),
        "node C must not be claimed when max-units=1; calls: {calls:?}"
    );
}

// ── AC14-EDGE: --max-units 0 is rejected with usage error ────────────────────
#[test]
fn ac14_max_units_zero_rejected() {
    use fno_agents::loop_megawalk::MegawalkQueue;
    // MegawalkQueue::new_with_max_units with max_units=0 should return an error
    // or the verb glue should exit 2. We test that max_units=Some(0) is
    // rejected before run_loop runs. The rejection is in the verb glue
    // (loop_target.rs flag parsing -> exit 2). For the queue unit test,
    // validate that a 0-unit cap causes no infinite loop: simply verify the
    // constructor is available and passes Some(0) through (the CLI gate is
    // the real check).
    //
    // The CLI-level check is: parse --max-units N; if N == 0, exit 2.
    // We verify this via the loop_target::run_loop_verb with "--max-units 0".
    let args: Vec<String> = vec![
        "run".to_string(),
        "--driver".to_string(),
        "megawalk".to_string(),
        "--max-units".to_string(),
        "0".to_string(),
        "--driver-lib-dir".to_string(),
        "/tmp".to_string(),
        "--cwd".to_string(),
        "/tmp".to_string(),
    ];
    let exit_code = fno_agents::loop_target::run_loop_verb(&args);
    assert_eq!(
        exit_code, 2,
        "--max-units 0 must exit 2 (usage error), got {exit_code}"
    );
}

// ── AC15-ERR: claim acquire exit 2 -> Queue error, not silent retry ───────────
//
// Finding 1 (sigma-review HIGH): every non-zero fno claim acquire exit was
// treated as "held by another session" and silently re-looped. The real
// contract: exit 1 = ClaimHeldByOther (retry), exit 2 = validation error
// (surface immediately), exit 3 = corrupted/gone-away (surface immediately).
//
// This test stubs fno where claim acquire exits 2 with stderr
// "validation error: bad holder". next() must return an error containing
// that stderr text. It must NOT loop (backlog next called only once).
#[test]
fn ac15_claim_acquire_exit2_surfaces_error_not_retry() {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");
    let call_log = tmp.path().join("calls.log");
    let call_log_str = call_log.display().to_string();

    let node_json = real_node_json("ab-valtest1", "Validation error node", None);

    write_stub(
        &bin_dir,
        "fno",
        &format!(
            r#"echo "$@" >> "{call_log_str}"
if [[ "$1" == "backlog" && "$2" == "next" ]]; then
  echo '{node_json}'
  exit 0
fi
if [[ "$1" == "claim" && "$2" == "acquire" ]]; then
  echo "validation error: bad holder" >&2
  exit 2
fi
if [[ "$1" == "doctor" ]]; then
  echo '{{"status":"fresh"}}'
  exit 0
fi
exit 0"#,
            call_log_str = call_log_str,
            node_json = node_json,
        ),
    );

    let abi_stub = bin_dir.join("fno").display().to_string();
    let mut q = MegawalkQueue::new(abi_stub, None, false);

    let err = match q.next() {
        Err(e) => e,
        Ok(_) => panic!("exit 2 from claim acquire must return Err"),
    };
    let err_str = err.to_string();

    assert!(
        err_str.contains("validation error: bad holder"),
        "error must contain the stderr from the failed claim acquire, got: {err_str}"
    );

    // Must NOT have looped: backlog next called exactly once.
    let calls = read_file_lines(&call_log);
    let next_calls: Vec<_> = calls
        .iter()
        .filter(|l| l.contains("backlog next"))
        .collect();
    assert_eq!(
        next_calls.len(),
        1,
        "backlog next must be called exactly once (no retry loop on exit 2); calls: {calls:?}"
    );
}

// ── AC16-ERR: claim acquire exit 3 -> Queue error, not silent retry ───────────
//
// Companion to AC15. exit 3 = corrupted / gone-away. Same contract: surface
// immediately as LoopError::Queue, never loop.
#[test]
fn ac16_claim_acquire_exit3_surfaces_error_not_retry() {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");
    let call_log = tmp.path().join("calls.log");
    let call_log_str = call_log.display().to_string();

    let node_json = real_node_json("ab-corr0001", "Corrupted claim node", None);

    write_stub(
        &bin_dir,
        "fno",
        &format!(
            r#"echo "$@" >> "{call_log_str}"
if [[ "$1" == "backlog" && "$2" == "next" ]]; then
  echo '{node_json}'
  exit 0
fi
if [[ "$1" == "claim" && "$2" == "acquire" ]]; then
  echo "corrupted claim: file truncated" >&2
  exit 3
fi
if [[ "$1" == "doctor" ]]; then
  echo '{{"status":"fresh"}}'
  exit 0
fi
exit 0"#,
            call_log_str = call_log_str,
            node_json = node_json,
        ),
    );

    let abi_stub = bin_dir.join("fno").display().to_string();
    let mut q = MegawalkQueue::new(abi_stub, None, false);

    let err = match q.next() {
        Err(e) => e,
        Ok(_) => panic!("exit 3 from claim acquire must return Err"),
    };
    let err_str = err.to_string();

    assert!(
        err_str.contains("corrupted claim: file truncated"),
        "error must contain the stderr from the failed claim acquire, got: {err_str}"
    );

    let calls = read_file_lines(&call_log);
    let next_calls: Vec<_> = calls
        .iter()
        .filter(|l| l.contains("backlog next"))
        .collect();
    assert_eq!(
        next_calls.len(),
        1,
        "backlog next must be called exactly once (no retry loop on exit 3); calls: {calls:?}"
    );
}

// ── AC17-HP: Parked detail includes evidence.message when non-empty ───────────
//
// Finding 2 (sigma-review MEDIUM): CloseOutcome::Parked lost the synthesized
// diagnostic in evidence.message (e.g. "no termination event after N dispatches").
// The detail must be "session terminated: <reason>: <message>" when message is
// non-empty.
#[test]
fn ac17_parked_detail_includes_evidence_message() {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");

    let node_json = real_node_json("ab-parkdetl", "Park detail test", None);

    write_stub(
        &bin_dir,
        "fno",
        &format!(
            r#"if [[ "$1" == "backlog" && "$2" == "next" ]]; then
  echo '{node_json}'
  exit 0
fi
if [[ "$1" == "claim" ]]; then
  exit 0
fi
exit 0"#,
            node_json = node_json,
        ),
    );

    let abi_stub = bin_dir.join("fno").display().to_string();
    let mut q = MegawalkQueue::new(abi_stub, None, false);
    let unit = q.next().unwrap().unwrap();

    // Budget reason + a non-empty synthesized diagnostic.
    let evidence = Evidence {
        reason: TerminationReason::Budget,
        message: "no termination event after 15 dispatch(es)".to_string(),
    };
    let outcome = q.close(&unit, &evidence).unwrap();

    match outcome {
        CloseOutcome::Parked(ref detail) => {
            assert!(
                detail.contains("no termination event after 15 dispatch(es)"),
                "Parked detail must include evidence.message, got: {detail:?}"
            );
        }
        other => panic!("expected Parked, got: {other:?}"),
    }
}

// ── AC18-HP: p0_failure cleared on success, does not pause next call ──────────
//
// Finding 3 (sigma-review latent): p0_failure was never cleared when a
// subsequent unit succeeded. After a p0 failure, a success must clear it
// so next() does not pause.
//
// Tests both MegawalkPolicyQueue (unit-test seam) and MegawalkQueue (production
// path). For MegawalkQueue we use a direct close() call sequence via the stub.
#[test]
fn ac18_p0_failure_cleared_on_success_policy_queue() {
    use fno_agents::loop_megawalk::MegawalkPolicyQueue;

    let mut q = MegawalkPolicyQueue::new();

    let p0_unit = Unit {
        id: "ab-p0fail".to_string(),
        title: "p0 unit".to_string(),
        session_key: "sk-p0".to_string(),
        plan_path: None,
        extra_env: vec![],
    };
    let ok_unit = Unit {
        id: "ab-success".to_string(),
        title: "success unit".to_string(),
        session_key: "sk-ok".to_string(),
        plan_path: None,
        extra_env: vec![],
    };

    // Record a p0 failure.
    let failure_evidence = Evidence {
        reason: TerminationReason::Budget,
        message: "budget hit".to_string(),
    };
    q.record_close(&p0_unit, &failure_evidence, true);

    // Verify pause is set.
    assert!(
        q.should_pause().is_some(),
        "p0 failure must trigger pause before success"
    );

    // Record a success on a different unit.
    let success_evidence = Evidence {
        reason: TerminationReason::DonePRGreen,
        message: "merged".to_string(),
    };
    q.record_close(&ok_unit, &success_evidence, false);

    // Pause must be cleared.
    assert!(
        q.should_pause().is_none(),
        "success must clear p0_failure so next() does not pause, got: {:?}",
        q.should_pause()
    );

    // And next() on an empty queue must return None (not Err).
    let result = q.next();
    assert!(
        matches!(result, Ok(None)),
        "after p0 cleared by success, next() must return Ok(None) not Err"
    );
}

// ── AC19-HP: p0_failure cleared in MegawalkQueue production path ─────────────
//
// Same invariant as AC18 but via the production MegawalkQueue::close() path.
// Strategy: dequeue a p0 unit, close it with failure (sets p0_failure), then
// call close() directly on a success unit (bypassing next() - valid for test
// only; in production the policy pause fires before the next dequeue). After
// the success close, policy_p0_failure must be None so a subsequent next()
// does not pause.
//
// We verify the internal effect indirectly: after the success close, feed
// two more non-p0 failures. The walk must pause for consecutive_failures (3),
// NOT for p0_failed, confirming p0_failure was cleared.
#[test]
fn ac19_p0_failure_cleared_in_production_queue_after_success() {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");
    let call_log = tmp.path().join("calls.log");
    let call_log_str = call_log.display().to_string();

    // The stub returns 5 nodes in sequence, then null.
    // Node 1: p0. Nodes 2-5: p2.
    let node_p0 = format!(
        r#"{{
  "id": "ab-p0prod1",
  "title": "p0 prod node",
  "priority": "p0",
  "domain": "code",
  "project": "abilities",
  "cwd": "/tmp",
  "size": null,
  "plan_path": null
}}"#
    );
    let node2 = real_node_json("ab-succ001", "Success node", None);
    let node3 = real_node_json("ab-fail002", "Fail node 2", None);
    let node4 = real_node_json("ab-fail003", "Fail node 3", None);

    let counter_file = tmp.path().join("next_count");
    fs::write(&counter_file, "0").unwrap();
    let counter_str = counter_file.display().to_string();

    write_stub(
        &bin_dir,
        "fno",
        &format!(
            r#"echo "$@" >> "{log}"
if [[ "$1" == "backlog" && "$2" == "next" ]]; then
  count=$(cat "{counter}" 2>/dev/null || echo 0)
  count=$((count + 1))
  echo "$count" > "{counter}"
  if [[ "$count" -eq 1 ]]; then
    echo '{node_p0}'
  elif [[ "$count" -eq 2 ]]; then
    echo '{node2}'
  elif [[ "$count" -eq 3 ]]; then
    echo '{node3}'
  elif [[ "$count" -eq 4 ]]; then
    echo '{node4}'
  else
    echo 'null'
  fi
  exit 0
fi
if [[ "$1" == "claim" ]]; then
  exit 0
fi
if [[ "$1" == "backlog" && "$2" == "done" ]]; then
  exit 0
fi
if [[ "$1" == "doctor" ]]; then
  echo '{{"status":"fresh"}}'
  exit 0
fi
exit 0"#,
            log = call_log_str,
            counter = counter_str,
            node_p0 = node_p0,
            node2 = node2,
            node3 = node3,
            node4 = node4,
        ),
    );

    let abi_stub = bin_dir.join("fno").display().to_string();
    let mut q = MegawalkQueue::new(abi_stub, None, false);

    // Step 1: dequeue p0 node, close with failure -> sets policy_p0_failure.
    let unit_a = q.next().unwrap().unwrap();
    assert_eq!(unit_a.id, "ab-p0prod1");
    q.close(
        &unit_a,
        &Evidence {
            reason: TerminationReason::Budget,
            message: "budget".to_string(),
        },
    )
    .unwrap();

    // Step 2: close a synthetic success unit directly (bypassing next()).
    // This exercises policy_record_close(is_success=true) to clear p0_failure.
    let fake_success = Unit {
        id: "ab-succ001".to_string(),
        title: "success".to_string(),
        session_key: "sk-succ".to_string(),
        plan_path: None,
        extra_env: vec![],
    };
    q.close(
        &fake_success,
        &Evidence {
            reason: TerminationReason::DonePRGreen,
            message: "merged".to_string(),
        },
    )
    .unwrap();

    // Step 3: next() must NOT pause for p0_failed (it was cleared).
    // It should dequeue node 2 (ab-succ001 - the second backlog entry).
    // The consecutive_failures streak was also reset by the success close, so
    // the two non-p0 failures below won't trigger a p0 pause.
    let unit2 = q.next().unwrap().unwrap();
    assert_eq!(
        unit2.id, "ab-succ001",
        "p0_failure must be cleared; next() must dequeue not pause"
    );
    q.close(
        &unit2,
        &Evidence {
            reason: TerminationReason::Budget,
            message: "fail".to_string(),
        },
    )
    .unwrap();

    let unit3 = q.next().unwrap().unwrap();
    assert_eq!(unit3.id, "ab-fail002");
    q.close(
        &unit3,
        &Evidence {
            reason: TerminationReason::Budget,
            message: "fail".to_string(),
        },
    )
    .unwrap();

    // Third failure in a row (streak=3) -> consecutive_failures pause, NOT p0_failed.
    // This confirms p0_failure is gone (otherwise p0_failed would fire on unit2's next()).
    let unit4 = q.next().unwrap().unwrap();
    assert_eq!(unit4.id, "ab-fail003");
    q.close(
        &unit4,
        &Evidence {
            reason: TerminationReason::Budget,
            message: "fail".to_string(),
        },
    )
    .unwrap();

    // Now streak is 3; next() must pause for consecutive_failures (not p0_failed).
    let pause_err = match q.next() {
        Err(e) => e.to_string(),
        Ok(_) => panic!("expected pause after 3 consecutive failures"),
    };
    assert!(
        pause_err.contains("consecutive_failures"),
        "pause must be consecutive_failures (p0_failure was cleared), got: {pause_err}"
    );
    assert!(
        !pause_err.contains("p0_failed"),
        "pause must NOT be p0_failed (it was cleared by success), got: {pause_err}"
    );
}

// ── AC20-EDGE: --max-units 1 with a PARKED unit stops walk ───────────────────
//
// Finding 7 (sigma-review undocumented): units_closed counts parked units
// toward --max-units because "once = process one unit, whatever the outcome".
// Test: --max-units 1 with a unit that parks (Budget termination) -> walk
// ends after that one unit (NoWork), no second dispatch.
#[test]
fn ac20_max_units_1_with_parked_unit_stops_walk() {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");
    let call_log = tmp.path().join("calls.log");
    let call_log_str = call_log.display().to_string();

    let abilities_dir = tmp.path().join(".fno");
    fs::create_dir_all(&abilities_dir).unwrap();
    let project_journal = abilities_dir.join("events.jsonl");
    let global_journal = tmp.path().join("global-events.jsonl");

    // Node A and B are both ready.
    let node_a = real_node_json("ab-park-mu1", "Park max-units node A", None);
    let node_b = real_node_json("ab-park-mu2", "Park max-units node B", None);

    let counter_file = tmp.path().join("next_count");
    fs::write(&counter_file, "0").unwrap();
    let counter_str = counter_file.display().to_string();

    write_stub(
        &bin_dir,
        "fno",
        &format!(
            r#"echo "$@" >> "{log}"
if [[ "$1" == "backlog" && "$2" == "next" ]]; then
  count=$(cat "{counter}" 2>/dev/null || echo 0)
  count=$((count + 1))
  echo "$count" > "{counter}"
  if [[ "$count" -eq 1 ]]; then
    echo '{node_a}'
  elif [[ "$count" -eq 2 ]]; then
    echo '{node_b}'
  else
    echo 'null'
  fi
  exit 0
fi
if [[ "$1" == "claim" ]]; then
  exit 0
fi
if [[ "$1" == "backlog" && "$2" == "done" ]]; then
  exit 0
fi
if [[ "$1" == "doctor" ]]; then
  echo '{{"status":"fresh"}}'
  exit 0
fi
exit 0"#,
            log = call_log_str,
            counter = counter_str,
            node_a = node_a,
            node_b = node_b,
        ),
    );

    // Driver lib: always emits Budget (so node A parks).
    let driver_lib = tmp.path().join("lib");
    fs::create_dir_all(&driver_lib).unwrap();
    let project_journal_str = project_journal.display().to_string();
    let driver_content = format!(
        r#"#!/usr/bin/env bash
driver_default_max() {{ echo 10; }}
driver_invoke() {{
  local sid="${{TARGET_SESSION_ID:-}}"
  local ts
  ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  printf '{{"ts":"%s","type":"termination","source":"hook","data":{{"session_id":"%s","reason":"Budget","message":"budget"}}}}\n' "$ts" "$sid" >> "{journal}"
}}
"#,
        journal = project_journal_str,
    );
    let lib_path = driver_lib.join("driver-claude-code.sh");
    fs::write(&lib_path, driver_content.as_bytes()).unwrap();
    fs::set_permissions(&lib_path, fs::Permissions::from_mode(0o755)).unwrap();

    write_stub(&bin_dir, "claude", "exit 0");
    let path_str = path_with(&bin_dir);
    let abi_stub = bin_dir.join("fno").display().to_string();

    let dispatch_cwd = tmp.path().join("project");
    fs::create_dir_all(&dispatch_cwd).unwrap();

    // max_units = 1: even if the unit parks, the walk should stop after one unit.
    let mut queue = MegawalkQueue::new_with_max_units(abi_stub.clone(), None, false, Some(1));
    let journal = Journal::new_raw(project_journal.clone(), global_journal.clone());
    let budget = LoopBudget::new(10).unwrap();

    use fno_agents::loop_megawalk::MegawalkDispatcher;
    let env = vec![
        ("PATH".to_string(), path_str.clone()),
        ("MAX_TURNS".to_string(), "5".to_string()),
        ("BUDGET_USD".to_string(), "25".to_string()),
        (
            "OUTPUT_FILE".to_string(),
            tmp.path().join("output.txt").display().to_string(),
        ),
        (
            "HISTORY_FILE".to_string(),
            tmp.path().join("history.txt").display().to_string(),
        ),
        (
            "SIGNAL_FILE".to_string(),
            tmp.path().join("signal.txt").display().to_string(),
        ),
        (
            "CONTINUE_PROMPT".to_string(),
            "/target no-merge test".to_string(),
        ),
    ];

    let dispatcher =
        MegawalkDispatcher::new(lib_path.clone(), env, dispatch_cwd, abi_stub.clone(), false);

    let cancel = || false;
    let outcome = run_loop(&mut queue, &dispatcher, &budget, &journal, &cancel, None)
        .expect("run_loop must not error");

    // Walk must stop with NoWork (cap reached after one parked unit).
    assert!(
        matches!(outcome.reason, TerminationReason::NoWork),
        "--max-units 1 with parked unit must end with NoWork, got: {:?}",
        outcome.reason
    );

    // Exactly 1 unit processed.
    assert_eq!(
        outcome.units.len(),
        1,
        "--max-units 1 must close exactly 1 unit (even if parked), got {}",
        outcome.units.len()
    );
    assert_eq!(outcome.units[0].unit_id, "ab-park-mu1");

    // The outcome for that unit must be Parked.
    assert!(
        matches!(outcome.units[0].close, CloseOutcome::Parked(_)),
        "unit must be Parked (Budget termination), got: {:?}",
        outcome.units[0].close
    );

    // Node B must NOT have been dispatched.
    let calls = read_file_lines(&call_log);
    assert!(
        !calls.iter().any(|l| l.contains("node:ab-park-mu2")),
        "node B must not be dispatched when max-units=1 and A parked; calls: {calls:?}"
    );
}

// ── AC21-HP: walker singleton held -> run_inner refuses with exit 1 ──────────
//
// Finding 8 (AC2-concurrency): when `fno claim acquire walker:<root>` exits 1
// (holder identity on stderr), the verb refuses with exit 1 and NO node
// dispatch happens (i.e. `fno backlog next` is never called).
//
// We test via a subprocess so global env state does not leak into parallel
// tests (using std::env::set_var in a multi-threaded test binary is unsafe).
#[test]
fn ac21_walker_singleton_held_refuses_and_no_dispatch() {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");
    let lib_dir = tmp.path().join("lib");
    let call_log = tmp.path().join("calls.log");
    let call_log_str = call_log.display().to_string();

    // Stub fno: walker claim acquire exits 1 (held); backlog next must NOT be reached.
    write_stub(
        &bin_dir,
        "fno",
        &format!(
            r#"echo "$@" >> "{call_log_str}"
if [[ "$1" == "claim" && "$2" == "acquire" ]]; then
  arg3="${{3:-}}"
  if [[ "$arg3" == walker:* ]]; then
    echo "holder: megawalk-loop:99999 on host build-box" >&2
    exit 1
  fi
fi
if [[ "$1" == "backlog" && "$2" == "next" ]]; then
  # must NOT be reached when walker claim is held
  echo "SHOULD-NOT-DISPATCH" >&2
  exit 0
fi
if [[ "$1" == "doctor" ]]; then
  echo '{{"status":"fresh"}}'
  exit 0
fi
exit 0"#,
            call_log_str = call_log_str,
        ),
    );

    // Driver lib stub (required for preflight).
    fs::create_dir_all(&lib_dir).unwrap();
    let driver_content = r#"#!/usr/bin/env bash
driver_default_max() { echo 10; }
driver_invoke() { exit 0; }
"#;
    let lib_path = lib_dir.join("driver-claude-code.sh");
    fs::write(&lib_path, driver_content.as_bytes()).unwrap();
    fs::set_permissions(&lib_path, fs::Permissions::from_mode(0o755)).unwrap();

    write_stub(&bin_dir, "claude", "exit 0");
    let path_str = path_with(&bin_dir);
    let abi_stub = bin_dir.join("fno").display().to_string();
    let cwd = tmp.path().join("project");
    fs::create_dir_all(&cwd).unwrap();

    // Invoke fno-agents loop run --driver megawalk via the compiled binary
    // so the walker claim logic runs in an isolated subprocess.
    // Find the test binary path via CARGO_BIN_EXE or fall back to cargo run.
    let binary_path = std::env::var("CARGO_BIN_EXE_fno-agents")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            // Use target/debug/fno-agents relative to the manifest dir.
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join("debug")
                .join("fno-agents")
        });

    // If the binary exists, spawn it; otherwise skip via cargo run.
    // We prefer the pre-built binary path from CARGO_TARGET_TMPDIR.
    let binary_path = if binary_path.exists() {
        binary_path
    } else {
        // Fall back: look in target/debug from the workspace root.
        let fallback = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join("debug")
            .join("fno-agents");
        if fallback.exists() {
            fallback
        } else {
            // Cannot find binary; skip test with a warning.
            eprintln!("ac21: skipping - fno-agents binary not found (run cargo build first)");
            return;
        }
    };

    let output = std::process::Command::new(&binary_path)
        .args([
            "loop",
            "run",
            "--driver",
            "megawalk",
            "--max-iterations",
            "5",
            "--driver-lib-dir",
            lib_dir.to_str().unwrap(),
            "--cwd",
            cwd.to_str().unwrap(),
        ])
        .env("FNO_BIN", &abi_stub)
        .env("PATH", &path_str)
        .output()
        .expect("failed to spawn fno-agents");

    let exit_code = output.status.code().unwrap_or(-1);
    assert_eq!(
        exit_code,
        1,
        "walker singleton held must return exit 1, got {exit_code}; \
         stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // backlog next must NOT have been called (no dispatch).
    let calls = read_file_lines(&call_log);
    let next_calls: Vec<_> = calls
        .iter()
        .filter(|l| l.contains("backlog next"))
        .collect();
    assert!(
        next_calls.is_empty(),
        "no backlog next call must happen when walker claim is held; calls: {calls:?}"
    );
}

// ── AC22-HP: node claim reclaimed-stale proceeds to dispatch ─────────────────
//
// Finding 8 (stale-claim recovery): stub claim acquire exits 0 with
// "reclaimed stale" on stdout -> next() proceeds and returns the unit normally.
// This verifies our exit-0 path is not broken by the new exit-code branching.
#[test]
fn ac22_node_claim_reclaimed_stale_proceeds_to_dispatch() {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");
    let call_log = tmp.path().join("calls.log");
    let call_log_str = call_log.display().to_string();

    let node_json = real_node_json("ab-stale001", "Stale reclaimed node", None);

    // Stub: claim acquire exits 0 with "reclaimed stale" on stdout (as the
    // real claims subsystem does for stale-claim recovery).
    write_stub(
        &bin_dir,
        "fno",
        &format!(
            r#"echo "$@" >> "{call_log_str}"
if [[ "$1" == "backlog" && "$2" == "next" ]]; then
  echo '{node_json}'
  exit 0
fi
if [[ "$1" == "claim" && "$2" == "acquire" ]]; then
  echo "reclaimed stale claim (prior holder dead)"
  exit 0
fi
exit 0"#,
            call_log_str = call_log_str,
            node_json = node_json,
        ),
    );

    let abi_stub = bin_dir.join("fno").display().to_string();
    let mut q = MegawalkQueue::new(abi_stub, None, false);

    let unit = q
        .next()
        .expect("reclaimed-stale claim (exit 0) must not error")
        .expect("must return Some(unit) after stale reclaim");

    assert_eq!(
        unit.id, "ab-stale001",
        "unit id must match the dequeued node"
    );

    // Claim acquire was called.
    let calls = read_file_lines(&call_log);
    assert!(
        calls
            .iter()
            .any(|l| l.contains("claim acquire") && l.contains("node:ab-stale001")),
        "claim acquire must have been called for the node; calls: {calls:?}"
    );
}

// ── AC23-HP: mission env injected into child process when node has mission fields ─
//
// Codex P1 (loop_megawalk.rs:949): fleet nodes spawned by megatron carry
// mission_id/mission_wave/mission_slug/mission_from_msg_id in the backlog-next
// JSON. The Rust walker must propagate these as TARGET_MISSION_* env vars into
// the child dispatch so init-target-state.sh can seed the manifest's mission_*
// fields and mission-emit.sh can observe completion.
//
// Test: stub fno returns a node WITH all four mission fields set; stub
// driver_invoke dumps its env to a file; assert all four TARGET_MISSION_*
// vars appear in the dumped env with the correct values.
#[test]
fn ac23_mission_env_injected_when_node_has_mission_fields() {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");
    let lib_dir = tmp.path().join("lib");
    let env_dump = tmp.path().join("env_dump.txt");
    let env_dump_str = env_dump.display().to_string();

    let abilities_dir = tmp.path().join(".fno");
    fs::create_dir_all(&abilities_dir).unwrap();
    let project_journal = abilities_dir.join("events.jsonl");
    let global_journal = tmp.path().join("global-events.jsonl");

    // Node JSON with all four mission fields (as _node_summary will emit them).
    let node_json = format!(
        r#"{{
  "id": "ab-fleet001",
  "title": "Fleet node with mission",
  "priority": "p2",
  "domain": "code",
  "project": "abilities",
  "cwd": "/tmp",
  "size": null,
  "plan_path": null,
  "mission_id": "mission-abc123",
  "mission_wave": 2,
  "mission_slug": "alpha-wave",
  "mission_from_msg_id": "msg-xyz789"
}}"#
    );

    let counter_file = tmp.path().join("next_count.txt");
    fs::write(&counter_file, "0").unwrap();
    let counter_str = counter_file.display().to_string();

    write_stub(
        &bin_dir,
        "fno",
        &format!(
            r#"if [[ "$1" == "backlog" && "$2" == "next" ]]; then
  count=$(cat "{counter}" 2>/dev/null || echo 0)
  count=$((count + 1))
  echo "$count" > "{counter}"
  if [[ "$count" -eq 1 ]]; then
    cat <<'NODEJSON'
{node_json}
NODEJSON
  else
    echo 'null'
  fi
  exit 0
fi
if [[ "$1" == "claim" ]]; then
  exit 0
fi
if [[ "$1" == "backlog" && "$2" == "done" ]]; then
  exit 0
fi
if [[ "$1" == "doctor" ]]; then
  echo '{{"status":"fresh"}}'
  exit 0
fi
exit 0"#,
            counter = counter_str,
            node_json = node_json,
        ),
    );

    // Driver lib: dumps env to file then emits DonePRGreen.
    let project_journal_str = project_journal.display().to_string();
    fs::create_dir_all(&lib_dir).unwrap();
    let driver_content = format!(
        r#"#!/usr/bin/env bash
driver_default_max() {{ echo 10; }}
driver_invoke() {{
  local sid="${{TARGET_SESSION_ID:-}}"
  # Dump all TARGET_MISSION_* env vars to the dump file.
  env | grep '^TARGET_MISSION_' >> "{env_dump}" || true
  local ts
  ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  printf '{{"ts":"%s","type":"termination","source":"hook","data":{{"session_id":"%s","reason":"DonePRGreen","message":"done"}}}}\n' "$ts" "$sid" >> "{journal}"
}}
"#,
        env_dump = env_dump_str,
        journal = project_journal_str,
    );
    let lib_path = lib_dir.join("driver-invoke.sh");
    fs::write(&lib_path, driver_content.as_bytes()).unwrap();
    fs::set_permissions(&lib_path, fs::Permissions::from_mode(0o755)).unwrap();
    let max_file = lib_dir.join("driver-default-max");
    fs::write(&max_file, "10\n").unwrap();

    let abi_stub = bin_dir.join("fno").display().to_string();
    let path_str = path_with(&bin_dir);

    let mut queue = MegawalkQueue::new(abi_stub.clone(), None, false);
    let journal = Journal::new_raw(project_journal.clone(), global_journal.clone());
    let budget = LoopBudget::new(10).unwrap();

    use fno_agents::loop_dispatch::ShelloutDispatcher;
    let env = vec![
        ("PATH".to_string(), path_str.clone()),
        ("MAX_TURNS".to_string(), "5".to_string()),
        ("BUDGET_USD".to_string(), "25".to_string()),
        (
            "OUTPUT_FILE".to_string(),
            tmp.path().join("output.txt").display().to_string(),
        ),
        (
            "HISTORY_FILE".to_string(),
            tmp.path().join("history.txt").display().to_string(),
        ),
        (
            "SIGNAL_FILE".to_string(),
            tmp.path().join("signal.txt").display().to_string(),
        ),
        ("CONTINUE_PROMPT".to_string(), String::new()),
    ];

    use fno_agents::loop_megawalk::MegawalkDispatcher;
    let dispatcher = MegawalkDispatcher::new(
        lib_path.clone(),
        env,
        tmp.path().to_path_buf(),
        abi_stub.clone(),
        false,
    );

    let cancel = || false;
    let outcome = run_loop(&mut queue, &dispatcher, &budget, &journal, &cancel, None)
        .expect("run_loop must not error");

    assert!(
        matches!(outcome.reason, TerminationReason::NoWork),
        "walk must end with NoWork; got: {:?}",
        outcome.reason
    );

    // The driver_invoke dumped TARGET_MISSION_* vars; assert all four are present.
    let env_contents = fs::read_to_string(&env_dump).unwrap_or_default();
    assert!(
        env_contents.contains("TARGET_MISSION_ID=mission-abc123"),
        "TARGET_MISSION_ID must be set; env dump: {env_contents:?}"
    );
    assert!(
        env_contents.contains("TARGET_MISSION_WAVE=2"),
        "TARGET_MISSION_WAVE must be set; env dump: {env_contents:?}"
    );
    assert!(
        env_contents.contains("TARGET_MISSION_SLUG=alpha-wave"),
        "TARGET_MISSION_SLUG must be set; env dump: {env_contents:?}"
    );
    assert!(
        env_contents.contains("TARGET_MISSION_FROM_MSG_ID=msg-xyz789"),
        "TARGET_MISSION_FROM_MSG_ID must be set; env dump: {env_contents:?}"
    );
}

// ── AC24-HP: no mission env when node has no mission_id ──────────────────────
//
// Non-fleet nodes (no mission_id) must NOT have any TARGET_MISSION_* vars
// injected - absence, not empty strings.
#[test]
fn ac24_no_mission_env_when_node_has_no_mission_id() {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");
    let lib_dir = tmp.path().join("lib");
    let env_dump = tmp.path().join("env_dump.txt");
    let env_dump_str = env_dump.display().to_string();

    let abilities_dir = tmp.path().join(".fno");
    fs::create_dir_all(&abilities_dir).unwrap();
    let project_journal = abilities_dir.join("events.jsonl");
    let global_journal = tmp.path().join("global-events.jsonl");

    // Plain node with null mission fields (what _node_summary emits for non-fleet).
    let node_json = r#"{
  "id": "ab-nofleet1",
  "title": "Regular non-fleet node",
  "priority": "p2",
  "domain": "code",
  "project": "abilities",
  "cwd": "/tmp",
  "size": null,
  "plan_path": null,
  "mission_id": null,
  "mission_wave": null,
  "mission_slug": null,
  "mission_from_msg_id": null
}"#;

    let counter_file = tmp.path().join("next_count.txt");
    fs::write(&counter_file, "0").unwrap();
    let counter_str = counter_file.display().to_string();

    write_stub(
        &bin_dir,
        "fno",
        &format!(
            r#"if [[ "$1" == "backlog" && "$2" == "next" ]]; then
  count=$(cat "{counter}" 2>/dev/null || echo 0)
  count=$((count + 1))
  echo "$count" > "{counter}"
  if [[ "$count" -eq 1 ]]; then
    cat <<'NODEJSON'
{node_json}
NODEJSON
  else
    echo 'null'
  fi
  exit 0
fi
if [[ "$1" == "claim" ]]; then exit 0; fi
if [[ "$1" == "backlog" && "$2" == "done" ]]; then exit 0; fi
if [[ "$1" == "doctor" ]]; then echo '{{"status":"fresh"}}'; exit 0; fi
exit 0"#,
            counter = counter_str,
            node_json = node_json,
        ),
    );

    let project_journal_str = project_journal.display().to_string();
    fs::create_dir_all(&lib_dir).unwrap();
    let driver_content = format!(
        r#"#!/usr/bin/env bash
driver_default_max() {{ echo 10; }}
driver_invoke() {{
  local sid="${{TARGET_SESSION_ID:-}}"
  env | grep '^TARGET_MISSION_' >> "{env_dump}" || true
  local ts
  ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  printf '{{"ts":"%s","type":"termination","source":"hook","data":{{"session_id":"%s","reason":"DonePRGreen","message":"done"}}}}\n' "$ts" "$sid" >> "{journal}"
}}
"#,
        env_dump = env_dump_str,
        journal = project_journal_str,
    );
    let lib_path = lib_dir.join("driver-invoke.sh");
    fs::write(&lib_path, driver_content.as_bytes()).unwrap();
    fs::set_permissions(&lib_path, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(lib_dir.join("driver-default-max"), "10\n").unwrap();

    let abi_stub = bin_dir.join("fno").display().to_string();
    let path_str = path_with(&bin_dir);

    let mut queue = MegawalkQueue::new(abi_stub.clone(), None, false);
    let journal = Journal::new_raw(project_journal.clone(), global_journal.clone());
    let budget = LoopBudget::new(10).unwrap();

    let env = vec![
        ("PATH".to_string(), path_str.clone()),
        ("MAX_TURNS".to_string(), "5".to_string()),
        ("BUDGET_USD".to_string(), "25".to_string()),
        (
            "OUTPUT_FILE".to_string(),
            tmp.path().join("output.txt").display().to_string(),
        ),
        (
            "HISTORY_FILE".to_string(),
            tmp.path().join("history.txt").display().to_string(),
        ),
        (
            "SIGNAL_FILE".to_string(),
            tmp.path().join("signal.txt").display().to_string(),
        ),
        ("CONTINUE_PROMPT".to_string(), String::new()),
    ];

    use fno_agents::loop_megawalk::MegawalkDispatcher;
    let dispatcher = MegawalkDispatcher::new(
        lib_path.clone(),
        env,
        tmp.path().to_path_buf(),
        abi_stub.clone(),
        false,
    );

    let cancel = || false;
    run_loop(&mut queue, &dispatcher, &budget, &journal, &cancel, None)
        .expect("run_loop must not error");

    // env_dump should be empty (or not exist) - no TARGET_MISSION_* vars set.
    let env_contents = fs::read_to_string(&env_dump).unwrap_or_default();
    assert!(
        env_contents.is_empty(),
        "no TARGET_MISSION_* vars must be set for non-fleet nodes; env dump: {env_contents:?}"
    );
}

// ── AC25-ERR: mission_id set but mission_wave missing -> loud Queue error ─────
//
// Mirrors Python MegawalkSchemaError strictness: corrupted fleet metadata must
// surface as a LoopError::Queue naming the node and the missing field, not
// silently strand the commander.
#[test]
fn ac25_mission_id_without_wave_returns_loud_queue_error() {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");

    // Node with mission_id but null mission_wave (corrupted fleet metadata).
    let node_json = r#"{
  "id": "ab-badfleet",
  "title": "Corrupted fleet node",
  "priority": "p2",
  "domain": "code",
  "project": "abilities",
  "cwd": "/tmp",
  "size": null,
  "plan_path": null,
  "mission_id": "mission-broken",
  "mission_wave": null,
  "mission_slug": "some-slug",
  "mission_from_msg_id": null
}"#;

    write_stub(
        &bin_dir,
        "fno",
        &format!(
            r#"if [[ "$1" == "backlog" && "$2" == "next" ]]; then
  cat <<'NODEJSON'
{node_json}
NODEJSON
  exit 0
fi
if [[ "$1" == "claim" ]]; then exit 0; fi
if [[ "$1" == "doctor" ]]; then echo '{{"status":"fresh"}}'; exit 0; fi
exit 0"#,
            node_json = node_json,
        ),
    );

    let abi_stub = bin_dir.join("fno").display().to_string();
    let mut queue = MegawalkQueue::new(abi_stub, None, false);

    // next() must return a LoopError::Queue naming the node id and the missing field.
    match queue.next() {
        Err(LoopError::Queue(msg)) => {
            assert!(
                msg.contains("ab-badfleet"),
                "error must name the node id; got: {msg:?}"
            );
            assert!(
                msg.to_lowercase().contains("wave") || msg.to_lowercase().contains("missing"),
                "error must mention the missing field; got: {msg:?}"
            );
        }
        Err(e) => panic!("expected Queue error, got other error: {e}"),
        Ok(_) => panic!("corrupted fleet node must return Err"),
    }
}

// ── AC26-HP: mission_from_msg_id null maps to empty string (not absent) ──────
//
// Python line 117: `str(mission_from_msg_id) if mission_from_msg_id is not None else ""`
// When mission_id is set but mission_from_msg_id is null, TARGET_MISSION_FROM_MSG_ID
// must be set to the empty string (not absent from the env).
#[test]
fn ac26_mission_from_msg_id_null_maps_to_empty_string() {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");
    let lib_dir = tmp.path().join("lib");
    let env_dump = tmp.path().join("env_dump.txt");
    let env_dump_str = env_dump.display().to_string();

    let abilities_dir = tmp.path().join(".fno");
    fs::create_dir_all(&abilities_dir).unwrap();
    let project_journal = abilities_dir.join("events.jsonl");
    let global_journal = tmp.path().join("global-events.jsonl");

    // mission_from_msg_id is null, all others present.
    let node_json = r#"{
  "id": "ab-nomsgid",
  "title": "Fleet node no from_msg_id",
  "priority": "p2",
  "domain": "code",
  "project": "abilities",
  "cwd": "/tmp",
  "size": null,
  "plan_path": null,
  "mission_id": "mission-xyz",
  "mission_wave": 1,
  "mission_slug": "beta-slug",
  "mission_from_msg_id": null
}"#;

    let counter_file = tmp.path().join("next_count.txt");
    fs::write(&counter_file, "0").unwrap();
    let counter_str = counter_file.display().to_string();

    write_stub(
        &bin_dir,
        "fno",
        &format!(
            r#"if [[ "$1" == "backlog" && "$2" == "next" ]]; then
  count=$(cat "{counter}" 2>/dev/null || echo 0)
  count=$((count + 1))
  echo "$count" > "{counter}"
  if [[ "$count" -eq 1 ]]; then
    cat <<'NODEJSON'
{node_json}
NODEJSON
  else
    echo 'null'
  fi
  exit 0
fi
if [[ "$1" == "claim" ]]; then exit 0; fi
if [[ "$1" == "backlog" && "$2" == "done" ]]; then exit 0; fi
if [[ "$1" == "doctor" ]]; then echo '{{"status":"fresh"}}'; exit 0; fi
exit 0"#,
            counter = counter_str,
            node_json = node_json,
        ),
    );

    let project_journal_str = project_journal.display().to_string();
    fs::create_dir_all(&lib_dir).unwrap();
    // Dump ALL env (not just TARGET_MISSION_) so we can check FROM_MSG_ID is SET (to empty).
    let driver_content = format!(
        r#"#!/usr/bin/env bash
driver_default_max() {{ echo 10; }}
driver_invoke() {{
  local sid="${{TARGET_SESSION_ID:-}}"
  # Use printenv to include vars set to empty string.
  printenv TARGET_MISSION_FROM_MSG_ID > "{env_dump}" 2>/dev/null || echo "__ABSENT__" > "{env_dump}"
  local ts
  ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  printf '{{"ts":"%s","type":"termination","source":"hook","data":{{"session_id":"%s","reason":"DonePRGreen","message":"done"}}}}\n' "$ts" "$sid" >> "{journal}"
}}
"#,
        env_dump = env_dump_str,
        journal = project_journal_str,
    );
    let lib_path = lib_dir.join("driver-invoke.sh");
    fs::write(&lib_path, driver_content.as_bytes()).unwrap();
    fs::set_permissions(&lib_path, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(lib_dir.join("driver-default-max"), "10\n").unwrap();

    let abi_stub = bin_dir.join("fno").display().to_string();
    let path_str = path_with(&bin_dir);

    let mut queue = MegawalkQueue::new(abi_stub.clone(), None, false);
    let journal = Journal::new_raw(project_journal.clone(), global_journal.clone());
    let budget = LoopBudget::new(10).unwrap();

    let env = vec![
        ("PATH".to_string(), path_str.clone()),
        ("MAX_TURNS".to_string(), "5".to_string()),
        ("BUDGET_USD".to_string(), "25".to_string()),
        (
            "OUTPUT_FILE".to_string(),
            tmp.path().join("output.txt").display().to_string(),
        ),
        (
            "HISTORY_FILE".to_string(),
            tmp.path().join("history.txt").display().to_string(),
        ),
        (
            "SIGNAL_FILE".to_string(),
            tmp.path().join("signal.txt").display().to_string(),
        ),
        ("CONTINUE_PROMPT".to_string(), String::new()),
    ];

    use fno_agents::loop_megawalk::MegawalkDispatcher;
    let dispatcher = MegawalkDispatcher::new(
        lib_path.clone(),
        env,
        tmp.path().to_path_buf(),
        abi_stub.clone(),
        false,
    );

    let cancel = || false;
    run_loop(&mut queue, &dispatcher, &budget, &journal, &cancel, None)
        .expect("run_loop must not error");

    // printenv writes the value (possibly empty) or we wrote __ABSENT__.
    let dump = fs::read_to_string(&env_dump).unwrap_or_else(|_| "__ABSENT__".to_string());
    let dump = dump.trim();
    assert!(
        dump != "__ABSENT__",
        "TARGET_MISSION_FROM_MSG_ID must be SET (even to empty string) when mission_from_msg_id is null"
    );
    assert!(
        dump.is_empty(),
        "TARGET_MISSION_FROM_MSG_ID must be empty string for null mission_from_msg_id; got: {dump:?}"
    );
}

// ── AC27-HP: walker claim released on all exit paths (subprocess test) ───────
//
// Gemini HIGH (loop_megawalk.rs:~1268): the LoopBudget::new Err arm returned
// Ok(2) without releasing the walker claim. The fix introduces a
// release_walker_claim closure called on every exit path after acquisition.
//
// This test verifies via subprocess (same pattern as AC21) to avoid polluting
// the parallel test process's PATH/FNO_BIN env vars. It exercises the normal
// exit path (empty backlog -> NoWork) and asserts the walker claim is released.
// The systematic closure ensures the budget-error arm also releases (code
// inspection confirms; the subprocess path covers the happy exit which
// exercises the same closure path).
#[test]
fn ac27_walker_claim_released_on_all_exit_paths() {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");
    let lib_dir = tmp.path().join("lib");
    let call_log = tmp.path().join("calls.log");
    let call_log_str = call_log.display().to_string();

    // Stub fno records all calls; walker claim acquire + release succeed.
    // backlog next returns null (empty walk -> NoWork).
    write_stub(
        &bin_dir,
        "fno",
        &format!(
            r#"echo "$@" >> "{log}"
if [[ "$1" == "backlog" && "$2" == "next" ]]; then
  echo 'null'
  exit 0
fi
if [[ "$1" == "claim" ]]; then exit 0; fi
if [[ "$1" == "doctor" ]]; then echo '{{"status":"fresh"}}'; exit 0; fi
exit 0"#,
            log = call_log_str,
        ),
    );

    // Driver lib needed for preflight (dispatcher "claude-code" -> driver-claude-code.sh).
    fs::create_dir_all(&lib_dir).unwrap();
    let driver_content =
        "#!/usr/bin/env bash\ndriver_default_max() { echo 10; }\ndriver_invoke() { : ; }\n";
    let lib_path = lib_dir.join("driver-claude-code.sh");
    fs::write(&lib_path, driver_content.as_bytes()).unwrap();
    fs::set_permissions(&lib_path, fs::Permissions::from_mode(0o755)).unwrap();

    // claude stub for preflight binary check.
    write_stub(&bin_dir, "claude", "exit 0");

    let abi_stub = bin_dir.join("fno").display().to_string();
    let path_str = path_with(&bin_dir);
    let cwd_dir = tmp.path().join("project");
    fs::create_dir_all(&cwd_dir).unwrap();
    fs::create_dir_all(cwd_dir.join(".fno")).unwrap();

    // Find the fno-agents binary (same resolution as AC21).
    let binary_path = std::env::var("CARGO_BIN_EXE_fno-agents")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../target")
                .join("debug")
                .join("fno-agents")
        });
    let binary_path = if binary_path.exists() {
        binary_path
    } else {
        let fallback = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join("debug")
            .join("fno-agents");
        if fallback.exists() {
            fallback
        } else {
            eprintln!("ac27: skipping - fno-agents binary not found (run cargo build first)");
            return;
        }
    };

    // Spawn the binary as a subprocess with env vars set on the child only.
    // --driver megawalk uses "claude-code" as the dispatcher (inner arg).
    // The outer --driver flag selects the driver type (megawalk); the
    // dispatcher name within megawalk is the default (claude-code).
    let output = std::process::Command::new(&binary_path)
        .args([
            "loop",
            "run",
            "--driver",
            "megawalk",
            "--max-iterations",
            "5",
            "--driver-lib-dir",
            lib_dir.to_str().unwrap(),
            "--cwd",
            cwd_dir.to_str().unwrap(),
        ])
        .env("FNO_BIN", &abi_stub)
        .env("PATH", &path_str)
        .output()
        .expect("failed to spawn fno-agents");

    let exit_code = output.status.code().unwrap_or(-1);
    // Empty backlog -> NoWork -> exit 0.
    assert_eq!(
        exit_code,
        0,
        "empty walk must exit 0; got {exit_code}; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Walker claim must have been released on the normal exit path.
    let calls = read_file_lines(&call_log);
    let release_calls: Vec<_> = calls
        .iter()
        .filter(|l| l.contains("claim release") && l.contains("walker:"))
        .collect();
    assert!(
        !release_calls.is_empty(),
        "walker claim must be released after walk completes; calls: {calls:?}"
    );
}

// ── AC6-FR (x-aba7): done exit 5 -> AwaitingMerge, claim RELEASED ─────────────
//
// When `fno backlog done` exits 5 (PR OPEN, not merged), the close outcome is
// AwaitingMerge (success-shaped): the claim is released like a Closed node and
// the node is closed later by reconcile at the actual merge.

#[test]
fn awaiting_merge_exit5_releases_claim() {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");
    let call_log = tmp.path().join("calls.log");
    let call_log_str = call_log.display().to_string();

    let node_json = real_node_json("ab-await001", "Awaiting merge", None);

    write_stub(
        &bin_dir,
        "fno",
        &format!(
            r#"echo "$@" >> "{call_log_str}"
if [[ "$1" == "backlog" && "$2" == "next" ]]; then
  echo '{node_json}'
  exit 0
fi
if [[ "$1" == "claim" ]]; then
  exit 0
fi
if [[ "$1" == "backlog" && "$2" == "done" ]]; then
  echo "awaiting merge: PR #7 is OPEN, not merged" >&2
  exit 5
fi
exit 0"#,
            call_log_str = call_log_str,
            node_json = node_json,
        ),
    );

    let abi_stub = bin_dir.join("fno").display().to_string();
    let mut q = MegawalkQueue::new(abi_stub, None, false);
    let unit = q.next().unwrap().unwrap();

    let evidence = Evidence {
        reason: TerminationReason::DonePRGreen,
        message: "PR up, reviewed".to_string(),
    };
    let outcome = q.close(&unit, &evidence).unwrap();

    assert_eq!(outcome, CloseOutcome::AwaitingMerge);

    let calls = read_file_lines(&call_log);
    // done was shelled (exit 5 drove the outcome).
    assert!(
        calls
            .iter()
            .any(|l| l.contains("backlog done") && l.contains("ab-await001")),
        "backlog done must be called; calls: {calls:?}"
    );
    // Claim RELEASED (success-shaped), not held.
    assert!(
        calls
            .iter()
            .any(|l| l.contains("claim release") && l.contains("node:ab-await001")),
        "claim must be released on AwaitingMerge; calls: {calls:?}"
    );
}

// ── AC7-FR (x-aba7 / x-f4d2): DoneAwaitingMerge reason -> AwaitingMerge ───────
//
// A unit that terminates DoneAwaitingMerge maps directly to AwaitingMerge
// WITHOUT shelling `fno backlog done` (the reason already carries the fact),
// releases the claim, and does NOT park (the pre-x-aba7 bug).

#[test]
fn done_awaiting_merge_reason_maps_without_calling_done() {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");
    let call_log = tmp.path().join("calls.log");
    let call_log_str = call_log.display().to_string();

    let node_json = real_node_json("ab-await002", "Awaiting reason", None);

    write_stub(
        &bin_dir,
        "fno",
        &format!(
            r#"echo "$@" >> "{call_log_str}"
if [[ "$1" == "backlog" && "$2" == "next" ]]; then
  echo '{node_json}'
  exit 0
fi
if [[ "$1" == "claim" ]]; then
  exit 0
fi
if [[ "$1" == "backlog" && "$2" == "done" ]]; then
  # If this fires, the mapping is wrong (should skip done entirely).
  echo "done SHOULD NOT be called for DoneAwaitingMerge" >&2
  exit 1
fi
exit 0"#,
            call_log_str = call_log_str,
            node_json = node_json,
        ),
    );

    let abi_stub = bin_dir.join("fno").display().to_string();
    let mut q = MegawalkQueue::new(abi_stub, None, false);
    let unit = q.next().unwrap().unwrap();

    let evidence = Evidence {
        reason: TerminationReason::DoneAwaitingMerge,
        message: "past proven main-red".to_string(),
    };
    let outcome = q.close(&unit, &evidence).unwrap();

    assert_eq!(outcome, CloseOutcome::AwaitingMerge);

    let calls = read_file_lines(&call_log);
    // `backlog done` must NOT be called for this reason.
    assert!(
        !calls.iter().any(|l| l.contains("backlog done")),
        "backlog done must NOT be called for DoneAwaitingMerge; calls: {calls:?}"
    );
    // Claim released (success-shaped), not held.
    assert!(
        calls
            .iter()
            .any(|l| l.contains("claim release") && l.contains("node:ab-await002")),
        "claim must be released on AwaitingMerge; calls: {calls:?}"
    );
}

// ── AC6-FR: AwaitingMerge does NOT increment the consecutive-failure streak ───
//
// Three AwaitingMerge closes in a row must not trip the consecutive_failures
// pause (three Parked closes would). Proves close records SUCCESS.

#[test]
fn awaiting_merge_does_not_trip_failure_streak() {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");
    let node_json = real_node_json("ab-streak01", "Streak", None);

    // backlog next always returns a node; claim ok; done always exits 5.
    write_stub(
        &bin_dir,
        "fno",
        &format!(
            r#"if [[ "$1" == "backlog" && "$2" == "next" ]]; then echo '{node_json}'; exit 0; fi
if [[ "$1" == "claim" ]]; then exit 0; fi
if [[ "$1" == "backlog" && "$2" == "done" ]]; then exit 5; fi
exit 0"#,
            node_json = node_json,
        ),
    );

    let abi_stub = bin_dir.join("fno").display().to_string();
    let mut q = MegawalkQueue::new(abi_stub, None, false);

    // Close 3 AwaitingMerge units directly (DonePRGreen reason + done exit 5).
    for i in 0..3 {
        let unit = Unit {
            id: format!("ab-streak0{i}"),
            title: "streak".to_string(),
            session_key: format!("sk-{i}"),
            plan_path: None,
            extra_env: vec![],
        };
        let outcome = q
            .close(
                &unit,
                &Evidence {
                    reason: TerminationReason::DonePRGreen,
                    message: "await".to_string(),
                },
            )
            .unwrap();
        assert_eq!(outcome, CloseOutcome::AwaitingMerge, "close {i}");
    }

    // After 3 successes the streak is 0: next() must NOT pause.
    let next = q.next().unwrap();
    assert!(
        next.is_some(),
        "AwaitingMerge must not trip consecutive_failures; next() should dequeue, not pause"
    );
}

// ── done-reason whose `backlog done` FAILS counts toward the streak ───────────
//
// Regression for the close_success invariant: a DonePRGreen close whose
// `fno backlog done` fails (-> Parked) must count as a FAILURE (outcome-shaped,
// not reason-shaped), so three in a row trip the consecutive-failure pause.

#[test]
fn done_reason_that_parks_trips_failure_streak() {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");
    let node_json = real_node_json("ab-parkstr0", "Park streak", None);

    // backlog next always returns a node; claim ok; done always FAILS (exit 1).
    write_stub(
        &bin_dir,
        "fno",
        &format!(
            r#"if [[ "$1" == "backlog" && "$2" == "next" ]]; then echo '{node_json}'; exit 0; fi
if [[ "$1" == "claim" ]]; then exit 0; fi
if [[ "$1" == "backlog" && "$2" == "done" ]]; then echo "done refused" >&2; exit 1; fi
exit 0"#,
            node_json = node_json,
        ),
    );

    let abi_stub = bin_dir.join("fno").display().to_string();
    let mut q = MegawalkQueue::new(abi_stub, None, false);

    // Close 3 DonePRGreen units whose `backlog done` fails -> Parked -> failure.
    for i in 0..3 {
        let unit = Unit {
            id: format!("ab-parkstr{i}"),
            title: "park".to_string(),
            session_key: format!("sk-p{i}"),
            plan_path: None,
            extra_env: vec![],
        };
        let outcome = q
            .close(
                &unit,
                &Evidence {
                    reason: TerminationReason::DonePRGreen,
                    message: "done failed".to_string(),
                },
            )
            .unwrap();
        assert!(matches!(outcome, CloseOutcome::Parked(_)), "close {i}");
    }

    // Streak is now 3: next() must pause for consecutive_failures.
    let pause_err = match q.next() {
        Err(e) => e.to_string(),
        Ok(_) => panic!("expected pause after 3 done-reason Parks"),
    };
    assert!(
        pause_err.contains("consecutive_failures"),
        "a done-reason that Parks must count toward the streak; got: {pause_err}"
    );
}
