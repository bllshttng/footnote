//! Wave B2 integration tests for `codex_ask::dispatch_codex_ask`.
//!
//! Uses a fake `codex` shell script that emits the JSONL contract:
//!   1. `{"type":"thread.started","thread_id":"<uuid>"}` (create only)
//!   2. `{"type":"item.completed","item":{"type":"agent_message","text":"<reply>"}}`
//!   3. `{"type":"turn.completed"}`
//!
//! The fake binary is injected via a temporary bin dir on PATH via std::process::Command::env.
//! `AgentsHome::at` pins the fno state dir. No daemon, no network.

use fno_agents::codex_ask::{dispatch_codex_ask, dispatch_codex_once};
use fno_agents::paths::AgentsHome;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn tmpdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "abi-codex-dispatch-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&p).unwrap();
    p
}

/// Install a fake `codex` binary in `bin_dir`.
///
/// The script emits a configurable JSONL sequence controlled by env vars:
/// - `FAKE_CODEX_SESSION_ID`: if set, emit a `thread.started` event with this UUID.
/// - `FAKE_CODEX_REPLY`: if set, emit an `item.completed/agent_message` with this text.
/// - `FAKE_CODEX_SOFT_ERROR`: if set, emit an `item.completed/error` with this message.
/// - `FAKE_CODEX_TURN_COMPLETE`: if set to "1", emit `turn.completed`.
/// - `FAKE_CODEX_EXIT`: if set, exit with this code (default 0).
/// - `FAKE_CODEX_NO_OUTPUT`: if set, emit nothing (simulates codex binary found but producing nothing useful).
/// - `FAKE_CODEX_EMPTY_SESSION`: if set, emit a `thread.started` with an EMPTY thread_id (cv-dcd823ce).
/// - `FAKE_CODEX_BAD_UTF8`: if set, emit an invalid-UTF-8 line then exit 0 WITHOUT turn.completed (cv-54a67325).
/// - `FAKE_CODEX_SLEEP`: if set, sleep this many seconds before turn.completed (timeout / SIGINT tests).
/// - `FAKE_CODEX_SIGINT_SENTINEL`: if set, trap SIGINT/SIGTERM, write "caught" to this path, exit 130 (ab-e7fdbcb6).
fn install_fake_codex(bin_dir: &Path) {
    let script = r#"#!/bin/sh
set -e
# Orphan-prevention test (ab-e7fdbcb6): record that a forwarded signal landed.
if [ -n "$FAKE_CODEX_SIGINT_SENTINEL" ]; then
  trap 'printf caught > "$FAKE_CODEX_SIGINT_SENTINEL"; exit 130' INT TERM
fi
if [ -n "$FAKE_CODEX_EXIT" ] && [ "$FAKE_CODEX_EXIT" != "0" ]; then
  # Emit nothing and exit with error code
  exit "$FAKE_CODEX_EXIT"
fi
if [ -z "$FAKE_CODEX_NO_OUTPUT" ]; then
  if [ -n "$FAKE_CODEX_EMPTY_SESSION" ]; then
    printf '{"type":"thread.started","thread_id":""}\n'
  elif [ -n "$FAKE_CODEX_SESSION_ID" ]; then
    printf '{"type":"thread.started","thread_id":"%s"}\n' "$FAKE_CODEX_SESSION_ID"
  fi
  if [ -n "$FAKE_CODEX_SOFT_ERROR" ]; then
    printf '{"type":"item.completed","item":{"type":"error","message":"%s"}}\n' "$FAKE_CODEX_SOFT_ERROR"
  fi
  if [ -n "$FAKE_CODEX_REPLY" ]; then
    printf '{"type":"item.completed","item":{"type":"agent_message","text":"%s"}}\n' "$FAKE_CODEX_REPLY"
  fi
  if [ -n "$FAKE_CODEX_BAD_UTF8" ]; then
    # Invalid UTF-8 lead bytes make Rust's BufRead::lines() yield Err(InvalidData);
    # exit WITHOUT turn.completed to simulate a mid-stream read error / truncation.
    printf '\377\376garbage\n'
    exit 0
  fi
  if [ -n "$FAKE_CODEX_SLEEP" ]; then
    sleep "$FAKE_CODEX_SLEEP"
  fi
  if [ "${FAKE_CODEX_TURN_COMPLETE:-1}" = "1" ]; then
    printf '{"type":"turn.completed"}\n'
  fi
fi
exit "${FAKE_CODEX_EXIT:-0}"
"#;
    let path = bin_dir.join("codex");
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Return a PATH string that puts `bin_dir` first.
fn path_with(bin_dir: &Path) -> String {
    format!("{}:/usr/bin:/bin:/usr/local/bin", bin_dir.display())
}

/// Module-level PATH mutex: every test that mutates the process-global PATH
/// must hold this lock for the duration of its mutation. Cargo runs tests in
/// parallel by default, so a per-test (function-scoped `static`) mutex does
/// NOT serialize against other tests that mutate PATH — they'd hold their own
/// mutexes and race on the shared global. Lift to module scope so all
/// PATH-mutating sites in this file serialize. (Caught by the parallel cargo
/// test run that surfaced `maybe_run_codex_ask_returns_some_for_codex_provider`
/// finding a leaked fake codex from a concurrent `dispatch_with_fake_codex`.)
static PATH_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Run `dispatch_codex_ask` with the fake codex binary on PATH.
/// This is done by temporarily overriding PATH via std::env::set_var for the duration
/// of the call. We use a mutex to prevent races between tests.
fn dispatch_with_fake_codex(
    home: &AgentsHome,
    bin_dir: &Path,
    name: &str,
    message: &str,
    from_name: &str,
    cwd: &Path,
    yolo: bool,
    timeout: Option<Duration>,
    fake_env: &[(&str, &str)],
) -> fno_agents::codex_ask::AskOutcome {
    // Hold the module-level PATH mutex so this PATH mutation does not race
    // with any other PATH-mutating test in this file.
    let _guard = PATH_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

    let old_path = std::env::var_os("PATH");
    let new_path = path_with(bin_dir);
    // SAFETY: single-threaded under the mutex; restored immediately after call.
    unsafe { std::env::set_var("PATH", &new_path) };
    for (k, v) in fake_env {
        unsafe { std::env::set_var(k, v) };
    }

    let result = dispatch_codex_ask(home, name, message, from_name, cwd, yolo, timeout);

    // Restore env
    match old_path {
        Some(p) => unsafe { std::env::set_var("PATH", p) },
        None => unsafe { std::env::remove_var("PATH") },
    }
    for (k, _v) in fake_env {
        unsafe { std::env::remove_var(k) };
    }
    result
}

/// Run `dispatch_codex_once` with the fake codex binary on PATH.
/// Used for all "create" tests after Task 1.3a (ask never creates; spawn --once does).
fn dispatch_once_with_fake_codex(
    home: &AgentsHome,
    bin_dir: &Path,
    name: &str,
    message: &str,
    from_name: &str,
    cwd: &Path,
    yolo: bool,
    timeout: Option<Duration>,
    fake_env: &[(&str, &str)],
) -> fno_agents::codex_ask::AskOutcome {
    let _guard = PATH_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

    let old_path = std::env::var_os("PATH");
    let new_path = path_with(bin_dir);
    unsafe { std::env::set_var("PATH", &new_path) };
    for (k, v) in fake_env {
        unsafe { std::env::set_var(k, v) };
    }

    let result = dispatch_codex_once(home, name, message, from_name, cwd, yolo, timeout);

    match old_path {
        Some(p) => unsafe { std::env::set_var("PATH", p) },
        None => unsafe { std::env::remove_var("PATH") },
    }
    for (k, _v) in fake_env {
        unsafe { std::env::remove_var(k) };
    }
    result
}

/// Seed a codex registry entry.
fn seed_codex_registry(home: &AgentsHome, name: &str, session_id: &str, cwd: &str) {
    let log_path = home
        .root()
        .join("agents")
        .join("logs")
        .join(format!("{}.jsonl", name));
    let log_path_str = log_path.to_string_lossy();
    let body = format!(
        r#"{{"schema_version":3,"agents":[{{"name":"{}","provider":"codex","cwd":"{}","codex_session_id":"{}","status":"live","created_at":"2026-05-27T00:00:00Z","log_path":"{}"}}]}}"#,
        name, cwd, session_id, log_path_str
    );
    let path = home.registry_json();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

// ---------------------------------------------------------------------------
// AC1-HP: create path (now spawn --once) - happy path
// ---------------------------------------------------------------------------

#[test]
fn codex_create_happy_path_returns_reply() {
    let home = AgentsHome::at(tmpdir("c1-home"));
    let bin_dir = tmpdir("c1-bin");
    let cwd = tmpdir("c1-cwd");
    install_fake_codex(&bin_dir);

    let outcome = dispatch_once_with_fake_codex(
        &home,
        &bin_dir,
        "myagent",
        "hello codex",
        "abilities",
        &cwd,
        false,
        Some(Duration::from_secs(10)),
        &[
            (
                "FAKE_CODEX_SESSION_ID",
                "aaaabbbb-1111-2222-3333-444455556666",
            ),
            ("FAKE_CODEX_REPLY", "hello back"),
        ],
    );
    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    assert_eq!(outcome.stdout, "hello back");
    // dispatch_codex_once emits a teardown receipt on stderr ("once: <name> torn down")
    assert!(
        outcome.stderr.contains("torn down"),
        "expected teardown receipt on stderr: {}",
        outcome.stderr
    );
}

#[test]
fn codex_create_writes_registry_entry_with_session_id() {
    let home = AgentsHome::at(tmpdir("c2-home"));
    let bin_dir = tmpdir("c2-bin");
    let cwd = tmpdir("c2-cwd");
    install_fake_codex(&bin_dir);

    let session_id = "bbbbcccc-1111-2222-3333-444455556666";
    let outcome = dispatch_once_with_fake_codex(
        &home,
        &bin_dir,
        "reg-agent",
        "test",
        "abilities",
        &cwd,
        false,
        Some(Duration::from_secs(10)),
        &[
            ("FAKE_CODEX_SESSION_ID", session_id),
            ("FAKE_CODEX_REPLY", "ok"),
        ],
    );
    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);

    // dispatch_codex_once tears the row down after the session completes, so
    // the registry should have NO live entry for reg-agent. The session_id
    // appears in the teardown receipt on stderr.
    assert!(
        outcome.stderr.contains(session_id),
        "teardown receipt should mention session_id: {}",
        outcome.stderr
    );
    let registry_path = home.registry_json();
    if registry_path.exists() {
        let body = fs::read_to_string(&registry_path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        let agents = v["agents"].as_array().map(|a| a.len()).unwrap_or(0);
        assert_eq!(agents, 0, "spawn --once must tear down the row: {}", body);
    }
}

#[test]
fn codex_create_tees_jsonl_to_log_file() {
    let home = AgentsHome::at(tmpdir("c3-home"));
    let bin_dir = tmpdir("c3-bin");
    let cwd = tmpdir("c3-cwd");
    install_fake_codex(&bin_dir);

    dispatch_once_with_fake_codex(
        &home,
        &bin_dir,
        "log-agent",
        "msg",
        "abilities",
        &cwd,
        false,
        Some(Duration::from_secs(10)),
        &[
            (
                "FAKE_CODEX_SESSION_ID",
                "ccccdddd-1111-2222-3333-444455556666",
            ),
            ("FAKE_CODEX_REPLY", "tee check"),
        ],
    );

    let log_path = home
        .root()
        .join("agents")
        .join("logs")
        .join("log-agent.jsonl");
    assert!(log_path.exists(), "output.jsonl tee should be created");
    let contents = fs::read_to_string(&log_path).unwrap();
    assert!(
        contents.contains("thread.started"),
        "tee should contain thread.started: {}",
        contents
    );
    assert!(
        contents.contains("turn.completed"),
        "tee should contain turn.completed: {}",
        contents
    );
}

// ---------------------------------------------------------------------------
// AC2-HP: resume path - happy path
// ---------------------------------------------------------------------------

#[test]
fn codex_resume_happy_path_returns_reply() {
    let home = AgentsHome::at(tmpdir("r1-home"));
    let bin_dir = tmpdir("r1-bin");
    let cwd = tmpdir("r1-cwd");
    install_fake_codex(&bin_dir);

    let session_id = "ddddeeee-1111-2222-3333-444455556666";
    seed_codex_registry(&home, "existing-agent", session_id, cwd.to_str().unwrap());

    let outcome = dispatch_with_fake_codex(
        &home,
        &bin_dir,
        "existing-agent",
        "follow up message",
        "abilities",
        &cwd,
        false,
        Some(Duration::from_secs(10)),
        &[
            // Resume does NOT emit thread.started (no session_id re-capture)
            ("FAKE_CODEX_REPLY", "resumed reply"),
        ],
    );
    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    assert_eq!(outcome.stdout, "resumed reply");
}

#[test]
fn codex_resume_bumps_last_message_at() {
    let home = AgentsHome::at(tmpdir("r2-home"));
    let bin_dir = tmpdir("r2-bin");
    let cwd = tmpdir("r2-cwd");
    install_fake_codex(&bin_dir);

    let session_id = "eeeeffff-1111-2222-3333-444455556666";
    seed_codex_registry(&home, "stamp-agent", session_id, cwd.to_str().unwrap());

    dispatch_with_fake_codex(
        &home,
        &bin_dir,
        "stamp-agent",
        "test",
        "abilities",
        &cwd,
        false,
        Some(Duration::from_secs(10)),
        &[("FAKE_CODEX_REPLY", "ok")],
    );

    let registry_body = fs::read_to_string(home.registry_json()).unwrap();
    assert!(
        registry_body.contains("last_message_at"),
        "last_message_at should be stamped: {}",
        registry_body
    );
}

// ---------------------------------------------------------------------------
// AC3-ERR: error paths
// ---------------------------------------------------------------------------

#[test]
fn codex_create_missing_binary_exits_14() {
    // No fake codex installed; PATH has only system dirs which don't have `codex`.
    let home = AgentsHome::at(tmpdir("e1-home"));
    let cwd = tmpdir("e1-cwd");

    // Use a bin_dir with nothing in it, so `codex` is not found.
    let empty_bin = tmpdir("e1-emptybin");

    let outcome = dispatch_once_with_fake_codex(
        &home,
        &empty_bin,
        "nobin-agent",
        "hi",
        "abilities",
        &cwd,
        false,
        Some(Duration::from_secs(5)),
        &[], // no fake env vars
    );
    // Python maps FileNotFoundError (127) to exit 14 on the create path
    assert_eq!(
        outcome.exit_code, 14,
        "missing binary should exit 14, got {} (stderr: {})",
        outcome.exit_code, outcome.stderr
    );
}

#[test]
fn codex_create_nonzero_exit_without_any_jsonl_exits_11() {
    // Python _run_codex: check expect_session BEFORE checking exit_code.
    // When codex exits non-zero with NO JSONL at all, NoSessionIdError fires
    // first (exit 11), not CodexInvocationError. This matches Python parity.
    let home = AgentsHome::at(tmpdir("e2-home"));
    let bin_dir = tmpdir("e2-bin");
    let cwd = tmpdir("e2-cwd");
    install_fake_codex(&bin_dir);

    let outcome = dispatch_once_with_fake_codex(
        &home,
        &bin_dir,
        "err-agent",
        "hi",
        "abilities",
        &cwd,
        false,
        Some(Duration::from_secs(5)),
        &[("FAKE_CODEX_EXIT", "3")], // exit 3, no JSONL emitted
    );
    // NoSessionIdError fires first (exit 11) because expect_session=true and
    // no thread.started was emitted before the no-JSONL subprocess died.
    // Python ordering: timeout -> no_session_id -> sigkill -> nonzero_exit.
    assert_eq!(
        outcome.exit_code, 11,
        "no JSONL + nonzero exit -> no_session_id check fires first (exit 11): {} (stderr: {})",
        outcome.exit_code, outcome.stderr
    );
}

#[test]
fn codex_create_no_session_id_exits_11() {
    let home = AgentsHome::at(tmpdir("e3-home"));
    let bin_dir = tmpdir("e3-bin");
    let cwd = tmpdir("e3-cwd");
    install_fake_codex(&bin_dir);

    // Emit turn.completed but NO thread.started -> NoSessionIdError -> exit 11
    let outcome = dispatch_once_with_fake_codex(
        &home,
        &bin_dir,
        "nosess-agent",
        "hi",
        "abilities",
        &cwd,
        false,
        Some(Duration::from_secs(5)),
        &[
            ("FAKE_CODEX_REPLY", "reply without session"),
            // No FAKE_CODEX_SESSION_ID
        ],
    );
    assert_eq!(
        outcome.exit_code, 11,
        "no session_id should exit 11: {} (stderr: {})",
        outcome.exit_code, outcome.stderr
    );
}

#[test]
fn codex_resume_missing_session_id_in_registry_exits_11() {
    let home = AgentsHome::at(tmpdir("e4-home"));
    let bin_dir = tmpdir("e4-bin");
    let cwd = tmpdir("e4-cwd");
    install_fake_codex(&bin_dir);

    // Registry entry has empty codex_session_id
    let body = r#"{"schema_version":3,"agents":[{"name":"badentry","provider":"codex","cwd":"/tmp","codex_session_id":"","status":"live","created_at":"2026-05-27T00:00:00Z","log_path":"/tmp/bad.jsonl"}]}"#;
    let path = home.registry_json();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();

    let outcome = dispatch_with_fake_codex(
        &home,
        &bin_dir,
        "badentry",
        "hi",
        "abilities",
        &cwd,
        false,
        Some(Duration::from_secs(5)),
        &[],
    );
    assert_eq!(
        outcome.exit_code, 11,
        "missing codex_session_id should exit 11: {} (stderr: {})",
        outcome.exit_code, outcome.stderr
    );
}

// ---------------------------------------------------------------------------
// AC4-EDGE: validation
// ---------------------------------------------------------------------------

#[test]
fn validation_empty_name_exits_2() {
    let home = AgentsHome::at(tmpdir("v1-home"));
    let cwd = tmpdir("v1-cwd");
    let outcome = dispatch_codex_ask(&home, "", "msg", "from", &cwd, false, None);
    assert_eq!(outcome.exit_code, 2);
}

#[test]
fn validation_empty_message_exits_2() {
    let home = AgentsHome::at(tmpdir("v2-home"));
    let cwd = tmpdir("v2-cwd");
    let outcome = dispatch_codex_ask(&home, "agent", "   ", "from", &cwd, false, None);
    assert_eq!(outcome.exit_code, 2);
}

#[test]
fn validation_empty_from_name_exits_2() {
    let home = AgentsHome::at(tmpdir("v3-home"));
    let cwd = tmpdir("v3-cwd");
    let outcome = dispatch_codex_ask(&home, "agent", "msg", "", &cwd, false, None);
    assert_eq!(outcome.exit_code, 2);
}

// ---------------------------------------------------------------------------
// AC5-EDGE: events.jsonl
// ---------------------------------------------------------------------------

#[test]
fn codex_create_emits_agent_ask_done_event() {
    let home = AgentsHome::at(tmpdir("ev1-home"));
    let bin_dir = tmpdir("ev1-bin");
    let cwd = tmpdir("ev1-cwd");
    install_fake_codex(&bin_dir);

    dispatch_once_with_fake_codex(
        &home,
        &bin_dir,
        "event-agent",
        "hello",
        "abilities",
        &cwd,
        false,
        Some(Duration::from_secs(10)),
        &[
            (
                "FAKE_CODEX_SESSION_ID",
                "ffffaaaa-1111-2222-3333-444455556666",
            ),
            ("FAKE_CODEX_REPLY", "reply"),
        ],
    );

    let events = fs::read_to_string(home.events_jsonl()).unwrap_or_default();
    assert!(
        events.contains("agent_ask_done"),
        "events.jsonl should contain agent_ask_done: {}",
        events
    );
    assert!(
        events.contains("codex"),
        "events.jsonl should show provider=codex: {}",
        events
    );
}

#[test]
fn codex_resume_emits_followup_events() {
    let home = AgentsHome::at(tmpdir("ev2-home"));
    let bin_dir = tmpdir("ev2-bin");
    let cwd = tmpdir("ev2-cwd");
    install_fake_codex(&bin_dir);

    let session_id = "aaaabbbb-9999-8888-7777-666655554444";
    seed_codex_registry(
        &home,
        "followup-ev-agent",
        session_id,
        cwd.to_str().unwrap(),
    );

    dispatch_with_fake_codex(
        &home,
        &bin_dir,
        "followup-ev-agent",
        "follow",
        "abilities",
        &cwd,
        false,
        Some(Duration::from_secs(10)),
        &[("FAKE_CODEX_REPLY", "followed")],
    );

    let events = fs::read_to_string(home.events_jsonl()).unwrap_or_default();
    assert!(
        events.contains("agent_followup_started"),
        "should emit followup_started: {}",
        events
    );
    assert!(
        events.contains("agent_followup_done"),
        "should emit followup_done: {}",
        events
    );
}

// ---------------------------------------------------------------------------
// AC6-EDGE: soft error item promotes to last_msg when no agent_message
// ---------------------------------------------------------------------------

#[test]
fn codex_create_soft_error_promotes_to_reply_when_no_agent_message() {
    let home = AgentsHome::at(tmpdir("se1-home"));
    let bin_dir = tmpdir("se1-bin");
    let cwd = tmpdir("se1-cwd");
    install_fake_codex(&bin_dir);

    let outcome = dispatch_once_with_fake_codex(
        &home,
        &bin_dir,
        "softerr-agent",
        "hello",
        "abilities",
        &cwd,
        false,
        Some(Duration::from_secs(10)),
        &[
            (
                "FAKE_CODEX_SESSION_ID",
                "softeeee-1111-2222-3333-444455556666",
            ),
            ("FAKE_CODEX_SOFT_ERROR", "hooks deprecated"),
            // No FAKE_CODEX_REPLY
        ],
    );
    // Exit 0 with soft error text promoted to reply (silent-failure-hunter row 5)
    assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
    assert_eq!(outcome.stdout, "hooks deprecated");
}

// ---------------------------------------------------------------------------
// AC7-EDGE: maybe_run_codex_ask provider routing
// ---------------------------------------------------------------------------

#[test]
fn maybe_run_codex_ask_returns_none_for_non_codex_provider() {
    use fno_agents::codex_ask::maybe_run_codex_ask;
    use serde_json::json;

    let home = AgentsHome::at(tmpdir("mr1-home"));
    let params = json!({"provider": "claude", "message": "hi", "from_name": "x"});
    let result = maybe_run_codex_ask(&home, &params, "myagent");
    assert!(result.is_none(), "should return None for provider=claude");
}

#[test]
fn maybe_run_codex_ask_returns_some_for_codex_provider() {
    use fno_agents::codex_ask::maybe_run_codex_ask;
    use serde_json::json;

    let home = AgentsHome::at(tmpdir("mr2-home"));
    let cwd = tmpdir("mr2-cwd");
    // Seed a codex registry entry so the unknown-name check passes.
    // With ask-never-creates (Task 1.3a), `maybe_run_codex_ask` on an unknown
    // name exits 16 rather than falling through to the binary; seed the row
    // so the test exercises the binary-invocation path (exit 14 = no binary).
    seed_codex_registry(&home, "new-agent", "sess-known-001", cwd.to_str().unwrap());

    // No fake codex installed -> will fail with exit 14, but returns Some
    let params = json!({"provider": "codex", "message": "hi", "from_name": "x"});

    // Hold the module-level PATH mutex so this PATH wipe does not race with
    // a concurrent `dispatch_with_fake_codex` that installs a fake codex on
    // PATH — that race lets the wipe see a leaked PATH and find codex when
    // the test asserts the binary is absent.
    let _guard = PATH_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let old_path = std::env::var_os("PATH");
    let empty = tmpdir("mr2-empty-bin");
    unsafe { std::env::set_var("PATH", path_with(&empty)) };

    let result = maybe_run_codex_ask(&home, &params, "new-agent");

    match old_path {
        Some(p) => unsafe { std::env::set_var("PATH", p) },
        None => unsafe { std::env::remove_var("PATH") },
    }

    assert!(result.is_some(), "should return Some for provider=codex");
    // Exit 14 because codex binary is absent (resume path also tries to invoke codex)
    assert_eq!(result.unwrap(), 14);
}

#[test]
fn maybe_run_codex_ask_mismatch_returns_exit_2() {
    use fno_agents::codex_ask::maybe_run_codex_ask;
    use serde_json::json;

    let home = AgentsHome::at(tmpdir("mr3-home"));
    let cwd = tmpdir("mr3-cwd");

    // Seed a codex agent
    seed_codex_registry(&home, "codex-agent", "sess-1234", cwd.to_str().unwrap());

    // Try to ask it with --provider gemini -> mismatch
    let params = json!({"provider": "gemini", "message": "hi", "from_name": "x"});
    let result = maybe_run_codex_ask(&home, &params, "codex-agent");
    assert_eq!(result, Some(2), "provider mismatch should return Some(2)");
}

// ---------------------------------------------------------------------------
// ab-cb15ae56: error-path correctness + observability hardening
// ---------------------------------------------------------------------------

/// cv-dcd823ce (CRITICAL): an empty `thread_id` on `thread.started` must NOT be
/// captured as the session id. The create path must fail closed with exit 11
/// (NoSessionId), and the registry must NOT gain an entry with an empty
/// `codex_session_id` (which would make every later resume fail opaquely).
#[test]
fn codex_create_empty_session_id_exits_11_and_writes_no_registry_entry() {
    let home = AgentsHome::at(tmpdir("hard-empty-home"));
    let bin_dir = tmpdir("hard-empty-bin");
    let cwd = tmpdir("hard-empty-cwd");
    install_fake_codex(&bin_dir);

    let outcome = dispatch_once_with_fake_codex(
        &home,
        &bin_dir,
        "empty-sess",
        "hi",
        "abilities",
        &cwd,
        false,
        Some(Duration::from_secs(5)),
        &[
            ("FAKE_CODEX_EMPTY_SESSION", "1"),
            ("FAKE_CODEX_REPLY", "ignored reply"),
        ],
    );

    assert_eq!(
        outcome.exit_code, 11,
        "empty thread_id must fail closed as NoSessionId (exit 11): {} (stderr: {})",
        outcome.exit_code, outcome.stderr
    );

    // No registry write should have happened (create failed before stamping).
    let registry_path = home.registry_json();
    if registry_path.exists() {
        let body = fs::read_to_string(&registry_path).unwrap();
        assert!(
            !body.contains("empty-sess"),
            "no registry entry should be written for a failed create: {}",
            body
        );
        assert!(
            !body.contains("\"codex_session_id\":\"\""),
            "must never persist an empty codex_session_id: {}",
            body
        );
    }
}

/// cv-54a67325 (HIGH): a genuine mid-stream read error (here an invalid-UTF-8
/// line) before `turn.completed` must NOT be swallowed as a silently-truncated
/// success. It surfaces as a non-zero exit with an observable stderr breadcrumb.
#[test]
fn codex_create_midstream_read_error_fails_loudly() {
    let home = AgentsHome::at(tmpdir("hard-utf8-home"));
    let bin_dir = tmpdir("hard-utf8-bin");
    let cwd = tmpdir("hard-utf8-cwd");
    install_fake_codex(&bin_dir);

    let outcome = dispatch_once_with_fake_codex(
        &home,
        &bin_dir,
        "utf8-agent",
        "hi",
        "abilities",
        &cwd,
        false,
        Some(Duration::from_secs(5)),
        &[
            (
                "FAKE_CODEX_SESSION_ID",
                "aaaabbbb-1111-2222-3333-444455556666",
            ),
            ("FAKE_CODEX_REPLY", "partial"),
            ("FAKE_CODEX_BAD_UTF8", "1"),
        ],
    );

    assert_ne!(
        outcome.exit_code, 0,
        "a truncated stream must not return success: stderr: {}",
        outcome.stderr
    );
    assert!(
        outcome.stderr.contains("stream read error"),
        "stderr should surface the read error: {}",
        outcome.stderr
    );
}

/// cv-9bc2abe7 (HIGH): a create-path timeout error must include the output.jsonl
/// path so the operator can find the partial reply codex captured before the
/// watchdog killed it. (Exit 15 = timeout.)
#[test]
fn codex_create_timeout_error_references_output_jsonl() {
    let home = AgentsHome::at(tmpdir("hard-timeout-home"));
    let bin_dir = tmpdir("hard-timeout-bin");
    let cwd = tmpdir("hard-timeout-cwd");
    install_fake_codex(&bin_dir);

    let outcome = dispatch_once_with_fake_codex(
        &home,
        &bin_dir,
        "timeout-agent",
        "hi",
        "abilities",
        &cwd,
        false,
        Some(Duration::from_secs(1)), // 1s timeout
        &[
            (
                "FAKE_CODEX_SESSION_ID",
                "ccccdddd-1111-2222-3333-444455556666",
            ),
            ("FAKE_CODEX_REPLY", "slow reply"),
            ("FAKE_CODEX_SLEEP", "4"), // sleep past the timeout
        ],
    );

    assert_eq!(
        outcome.exit_code, 15,
        "wall-clock timeout should exit 15: {} (stderr: {})",
        outcome.exit_code, outcome.stderr
    );
    assert!(
        outcome.stderr.contains(".jsonl") && outcome.stderr.contains("details"),
        "timeout error should reference the output.jsonl log path: {}",
        outcome.stderr
    );
}

// NOTE: the SIGINT-forwarding end-to-end test lives in its OWN test binary
// (tests/codex_ask_sigint.rs). It self-delivers SIGINT to the test process,
// and the SIGINT-forwarding handler is process-global; isolating it in a
// single-test binary guarantees no other test in this file's process is
// running (and thus exposed to the default SIGINT disposition) when the
// signal lands. (Sigma-review concurrency concern; this file has prior CI
// race history — commit e3058a3d.)
