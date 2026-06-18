/// Integration tests for loop_megatron.rs (MegatronQueue + MegatronDispatcher)
/// and the group-3 megawalk seams (--mission passthrough, walk termination
/// emission).
///
/// Tests use stub `fno` / `fno-agents` scripts that record their argv; no
/// real fleet directory, backlog graph, or network involved. The full
/// recursion (real binary re-invoking itself with --driver megawalk) is
/// covered by tests/smoke-megatron-e2e.sh.
use fno_agents::loop_megatron::{MegatronDispatcher, MegatronQueue};
use fno_agents::loop_megawalk::{emit_walk_termination, MegawalkQueue};
use fno_agents::loop_runtime::{
    CloseOutcome, DispatchCtx, Dispatcher, Evidence, Journal, LoopError, Queue,
};
use fno_agents::loopcheck::TerminationReason;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

// ── helpers ───────────────────────────────────────────────────────────────────

fn write_stub(dir: &Path, name: &str, body: &str) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    let content = format!("#!/usr/bin/env bash\n{body}\n");
    fs::write(&path, content.as_bytes()).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

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

/// Unit JSON in the `fno megatron next --json` shape.
///
/// Contract note: the Rust parser reads `project`, `wave`, `project_path`,
/// and `title`; `node_id` / `mission_id` / `slug` are contract fields carried
/// for the Python side and operators, included here so the fixture mirrors
/// the real wire shape (cli/tests/megatron/test_queue.py pins the emitter).
fn unit_json(project: &str, wave: u64, project_path: &str) -> String {
    format!(
        r#"{{"project": "{project}", "wave": {wave}, "project_path": "{project_path}", "node_id": "ab-fk000001", "title": "Mission ab-mt0001 wave {wave} - {project}", "mission_id": "ab-mt0001", "slug": "2026-06-07-test"}}"#
    )
}

// ── MegatronQueue::next ───────────────────────────────────────────────────────

#[test]
fn next_parses_unit_and_populates_extra_env() {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");
    let stub = write_stub(
        &bin_dir,
        "fno",
        &format!("echo '{}'", unit_json("backend", 1, "/tmp/proj/backend")),
    );

    let mut q = MegatronQueue::new(stub.display().to_string(), "ab-mt0001".to_string());
    let unit = q.next().expect("no error").expect("expected Some(unit)");

    assert_eq!(unit.id, "backend@wave-1");
    assert_eq!(unit.title, "Mission ab-mt0001 wave 1 - backend");
    assert!(
        unit.session_key.contains("mt"),
        "session_key should carry the mt infix: {}",
        unit.session_key
    );
    let env: std::collections::HashMap<_, _> = unit.extra_env.iter().cloned().collect();
    assert_eq!(
        env.get("MEGATRON_PROJECT_PATH").map(String::as_str),
        Some("/tmp/proj/backend")
    );
    assert_eq!(
        env.get("MEGATRON_PROJECT").map(String::as_str),
        Some("backend")
    );
    assert_eq!(env.get("MEGATRON_WAVE").map(String::as_str), Some("1"));
    assert_eq!(
        env.get("MEGATRON_MISSION_ID").map(String::as_str),
        Some("ab-mt0001")
    );
}

#[test]
fn next_null_means_mission_complete() {
    let tmp = TempDir::new().unwrap();
    let stub = write_stub(&tmp.path().join("bin"), "fno", "echo 'null'");

    let mut q = MegatronQueue::new(stub.display().to_string(), "ab-mt0001".to_string());
    assert!(q.next().expect("no error").is_none());
}

#[test]
fn next_pause_maps_to_typed_pause_error() {
    let tmp = TempDir::new().unwrap();
    let stub = write_stub(
        &tmp.path().join("bin"),
        "fno",
        r#"echo '{"pause": {"policy": "manifest_mutated", "detail": "stored_sha=abc fresh_sha=def"}}'"#,
    );

    let mut q = MegatronQueue::new(stub.display().to_string(), "ab-mt0001".to_string());
    match q.next() {
        // AC4-FR: the typed variant carries policy + detail as named fields, and
        // detail with an embedded `=`/space passes through verbatim (no parse).
        Err(LoopError::Pause { policy, detail }) => {
            assert_eq!(policy, "manifest_mutated");
            assert_eq!(detail, "stored_sha=abc fresh_sha=def");
        }
        Err(other) => panic!("expected LoopError::Pause, got {other:?}"),
        Ok(_) => panic!("expected LoopError::Pause, got Ok"),
    }
}

#[test]
fn next_missing_project_path_is_loud() {
    let tmp = TempDir::new().unwrap();
    let stub = write_stub(
        &tmp.path().join("bin"),
        "fno",
        r#"echo '{"project": "ghost", "wave": 1, "project_path": null}'"#,
    );

    let mut q = MegatronQueue::new(stub.display().to_string(), "ab-mt0001".to_string());
    match q.next() {
        Err(LoopError::Queue(msg)) => {
            assert!(msg.contains("ghost"), "error must name the project: {msg}");
            assert!(
                msg.contains("project_path"),
                "error must name the missing field: {msg}"
            );
        }
        Err(other) => panic!("expected Queue error, got {other:?}"),
        Ok(_) => panic!("expected Queue error, got Ok"),
    }
}

#[test]
fn next_verb_failure_fails_closed() {
    let tmp = TempDir::new().unwrap();
    let stub = write_stub(
        &tmp.path().join("bin"),
        "fno",
        "echo 'mission not found' >&2; exit 2",
    );

    let mut q = MegatronQueue::new(stub.display().to_string(), "ab-mt0001".to_string());
    match q.next() {
        Err(LoopError::Queue(msg)) => {
            assert!(msg.contains("megatron next"), "error names the verb: {msg}");
        }
        Err(other) => panic!("expected Queue error, got {other:?}"),
        Ok(_) => panic!("expected Queue error, got Ok"),
    }
}

// ── MegatronQueue::close ──────────────────────────────────────────────────────

/// Build a stub fno that logs argv and run a next() + close() cycle with the
/// given termination reason. Returns (close outcome, logged argv lines).
fn close_cycle(
    reason: TerminationReason,
    complete_exit: i32,
) -> (Result<CloseOutcome, LoopError>, Vec<String>) {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");
    let call_log = tmp.path().join("calls.log");
    let stub = write_stub(
        &bin_dir,
        "fno",
        &format!(
            r#"echo "$@" >> "{log}"
if [[ "$1" == "megatron" && "$2" == "next" ]]; then
  echo '{unit}'
  exit 0
fi
if [[ "$1" == "megatron" && "$2" == "complete" ]]; then
  echo '{{"result": "recorded"}}'
  exit {complete_exit}
fi
exit 0"#,
            log = call_log.display(),
            unit = unit_json("backend", 2, "/tmp/proj/backend"),
            complete_exit = complete_exit,
        ),
    );

    let mut q = MegatronQueue::new(stub.display().to_string(), "ab-mt0001".to_string());
    let unit = q.next().expect("no error").expect("expected Some(unit)");
    let evidence = Evidence {
        reason,
        message: "walk summary".to_string(),
    };
    let out = q.close(&unit, &evidence);
    (out, read_file_lines(&call_log))
}

#[test]
fn close_nowork_records_done_and_closes() {
    let (out, calls) = close_cycle(TerminationReason::NoWork, 0);

    assert!(matches!(out, Ok(CloseOutcome::Closed)), "got {out:?}");
    let complete_call = calls
        .iter()
        .find(|l| l.contains("megatron complete"))
        .expect("complete not called");
    assert!(
        complete_call.contains("--project backend"),
        "{complete_call}"
    );
    assert!(complete_call.contains("--wave 2"), "{complete_call}");
    assert!(complete_call.contains("--outcome done"), "{complete_call}");
    assert!(complete_call.contains("--reason NoWork"), "{complete_call}");
}

#[test]
fn close_budget_records_failed_and_parks() {
    let (out, calls) = close_cycle(TerminationReason::Budget, 0);

    match out {
        Ok(CloseOutcome::Parked(detail)) => {
            assert!(detail.contains("Budget"), "detail carries reason: {detail}");
            assert!(
                detail.contains("walk summary"),
                "detail carries evidence message: {detail}"
            );
        }
        other => panic!("expected Parked, got {other:?}"),
    }
    let complete_call = calls
        .iter()
        .find(|l| l.contains("megatron complete"))
        .expect("complete not called");
    assert!(
        complete_call.contains("--outcome failed"),
        "{complete_call}"
    );
}

#[test]
fn close_done_incomplete_result_parks() {
    // codex P1 (PR #458): the complete verb can refuse a done outcome when
    // the graph says the node is not done. The queue must map the
    // {"result": "incomplete"} answer to Parked, never Closed.
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");
    let stub = write_stub(
        &bin_dir,
        "fno",
        &format!(
            r#"if [[ "$1" == "megatron" && "$2" == "next" ]]; then
  echo '{unit}'
  exit 0
fi
if [[ "$1" == "megatron" && "$2" == "complete" ]]; then
  echo '{{"result": "incomplete", "detail": "node ab-x is ready (claim held elsewhere)"}}'
  exit 0
fi
exit 0"#,
            unit = unit_json("backend", 1, "/tmp/proj/backend"),
        ),
    );

    let mut q = MegatronQueue::new(stub.display().to_string(), "ab-mt0001".to_string());
    let unit = q.next().expect("no error").expect("expected Some(unit)");
    let evidence = Evidence {
        reason: TerminationReason::NoWork,
        message: String::new(),
    };

    match q.close(&unit, &evidence) {
        Ok(CloseOutcome::Parked(detail)) => {
            assert!(detail.contains("claim held elsewhere"), "{detail}");
        }
        other => panic!("expected Parked on incomplete, got {other:?}"),
    }
}

#[test]
fn close_verb_failure_fails_closed() {
    let (out, _calls) = close_cycle(TerminationReason::NoWork, 4);

    match out {
        Err(LoopError::Queue(msg)) => {
            assert!(msg.contains("megatron complete"), "names the verb: {msg}");
        }
        other => panic!("expected Queue error (fail closed), got {other:?}"),
    }
}

// ── MegatronDispatcher ────────────────────────────────────────────────────────

#[test]
fn dispatcher_spawns_child_megawalk_with_recursion_flags() {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");
    let call_log = tmp.path().join("agent_calls.log");
    // Stub fno-agents: log argv, exit 0.
    let agents_stub = write_stub(
        &bin_dir,
        "fno-agents",
        &format!(r#"echo "$@" >> "{}""#, call_log.display()),
    );

    // Build the unit via a stub queue cycle so extra_env is realistic.
    let abi_stub = write_stub(
        &bin_dir,
        "fno",
        &format!("echo '{}'", unit_json("backend", 1, "/tmp/proj/backend")),
    );
    let mut q = MegatronQueue::new(abi_stub.display().to_string(), "ab-mt0001".to_string());
    let unit = q.next().expect("no error").expect("expected Some(unit)");

    let dispatcher = MegatronDispatcher::new(
        agents_stub,
        "claude-code".to_string(),
        tmp.path().join("lib"),
        "ab-mt0001".to_string(),
        7,
        12.5,
        Some("opus".to_string()),
        None,
        false,
    );

    let mut session = dispatcher
        .run(&unit, &DispatchCtx { iteration: 1 })
        .expect("dispatch must succeed");
    let code = session.wait().expect("wait must succeed");
    assert_eq!(code, 0);

    let calls = read_file_lines(&call_log);
    assert_eq!(calls.len(), 1, "exactly one child spawn");
    let argv = &calls[0];
    assert!(argv.contains("loop run"), "{argv}");
    assert!(argv.contains("--driver megawalk"), "recursion: {argv}");
    assert!(argv.contains("--cwd /tmp/proj/backend"), "{argv}");
    assert!(argv.contains("--mission ab-mt0001"), "{argv}");
    assert!(
        argv.contains(&format!("--termination-key {}", unit.session_key)),
        "child must journal a termination keyed to the unit: {argv}"
    );
    assert!(argv.contains("--max-turns 7"), "{argv}");
    assert!(argv.contains("--budget 12.5"), "{argv}");
    assert!(argv.contains("--model opus"), "{argv}");
    assert!(
        !argv.contains("--allow-merge"),
        "allow_merge=false must not pass the flag: {argv}"
    );
}

#[test]
fn next_mints_fresh_session_keys_across_calls() {
    // A commander restart re-dispatching the same project must mint a NEW
    // session key, so the runtime resume guard can never cross-match a stale
    // child termination from a prior commander generation. Re-dispatch
    // idempotency rests on the completion-record ledger, not the journal.
    let tmp = TempDir::new().unwrap();
    let stub = write_stub(
        &tmp.path().join("bin"),
        "fno",
        &format!("echo '{}'", unit_json("backend", 1, "/tmp/proj/backend")),
    );

    let mut q = MegatronQueue::new(stub.display().to_string(), "ab-mt0001".to_string());
    let first = q.next().unwrap().unwrap();
    let second = q.next().unwrap().unwrap();

    assert_ne!(
        first.session_key, second.session_key,
        "successive dispatches of the same project must not share a session key"
    );
}

#[test]
fn close_malformed_wave_in_fallback_is_loud() {
    // The close() fallback parse (no active entry) must reject a non-numeric
    // wave rather than silently recording --wave 0 (a record no manifest
    // matches; the mission would stall instead of failing).
    let tmp = TempDir::new().unwrap();
    let stub = write_stub(&tmp.path().join("bin"), "fno", "exit 0");

    let mut q = MegatronQueue::new(stub.display().to_string(), "ab-mt0001".to_string());
    let unit = fno_agents::loop_runtime::Unit {
        id: "backend@wave-NaN".to_string(),
        title: "t".to_string(),
        session_key: "k".to_string(),
        plan_path: None,
        extra_env: vec![],
    };
    let evidence = Evidence {
        reason: TerminationReason::NoWork,
        message: String::new(),
    };

    match q.close(&unit, &evidence) {
        Err(LoopError::Queue(msg)) => {
            assert!(msg.contains("malformed unit id"), "{msg}");
        }
        other => panic!("expected loud Queue error, got {other:?}"),
    }
}

// ── megawalk seams (group 3) ──────────────────────────────────────────────────

#[test]
fn megawalk_queue_passes_mission_filter_through() {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");
    let call_log = tmp.path().join("calls.log");
    let stub = write_stub(
        &bin_dir,
        "fno",
        &format!(
            r#"echo "$@" >> "{}"
echo 'null'"#,
            call_log.display()
        ),
    );

    let mut q = MegawalkQueue::new(stub.display().to_string(), None, false)
        .with_mission(Some("ab-mt0001".to_string()));
    assert!(q.next().expect("no error").is_none());

    let calls = read_file_lines(&call_log);
    let next_call = calls
        .iter()
        .find(|l| l.contains("backlog next"))
        .expect("backlog next not called");
    assert!(
        next_call.contains("--mission ab-mt0001"),
        "mission filter must pass through: {next_call}"
    );
}

#[test]
fn emit_walk_termination_round_trips_through_find_termination() {
    let tmp = TempDir::new().unwrap();
    let project_journal = tmp.path().join("project-events.jsonl");
    let global_journal = tmp.path().join("global-events.jsonl");
    let journal = Journal::new_raw(project_journal.clone(), global_journal);

    emit_walk_termination(
        &journal,
        "20260607T000000Z-mt123-abcdef",
        &TerminationReason::NoWork,
        3,
        2,
    )
    .expect("emit must succeed");

    let evidence = journal
        .find_termination("20260607T000000Z-mt123-abcdef")
        .expect("scan must succeed")
        .expect("termination event must be found");
    assert!(matches!(evidence.reason, TerminationReason::NoWork));
    assert!(
        evidence.message.contains("3 iterations"),
        "message carries the walk summary: {}",
        evidence.message
    );

    // The event must be the canonical envelope shape with source "loop".
    let line = &read_file_lines(&project_journal)[0];
    let v: serde_json::Value = serde_json::from_str(line).unwrap();
    assert_eq!(v["type"].as_str(), Some("termination"));
    assert_eq!(v["source"].as_str(), Some("loop"));
    assert_eq!(v["data"]["reason"].as_str(), Some("NoWork"));
}

#[test]
fn find_termination_falls_back_to_global_for_mt_keys() {
    // The load-bearing recursion seam: the commander's project journal does
    // NOT contain the child walk's termination (the child ran in a different
    // cwd); only the global mirror does. find_termination must find the
    // mt-keyed event through the global fallback alone.
    let tmp = TempDir::new().unwrap();
    let commander_journal = tmp.path().join("commander-events.jsonl");
    let global_journal = tmp.path().join("global-events.jsonl");

    // Seed the GLOBAL journal only (commander project journal absent).
    fs::write(
        &global_journal,
        "{\"ts\":\"2026-06-07T00:00:00Z\",\"type\":\"termination\",\"source\":\"loop\",\
         \"data\":{\"session_id\":\"20260607T000000Z-mt42-cafe01\",\"reason\":\"NoWork\",\
         \"message\":\"child walk done\"}}\n",
    )
    .unwrap();

    let journal = Journal::new_raw(commander_journal.clone(), global_journal);
    assert!(
        !commander_journal.exists(),
        "precondition: project journal absent"
    );

    let evidence = journal
        .find_termination("20260607T000000Z-mt42-cafe01")
        .expect("scan must succeed")
        .expect("mt-keyed termination must be found via the global fallback");
    assert!(matches!(evidence.reason, TerminationReason::NoWork));
}
