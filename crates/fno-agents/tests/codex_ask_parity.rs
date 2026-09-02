//! parity-stage: characterization
//! parity-oracle: fno.agents.harnesses.codex.resume
//!
//! Wave B3: codex ask cross-language parity harness (ab-0429c6e1), now a
//! characterization suite for the `resume` ask leg (the ask-adapter port).
//!
//! The resume happy path was frozen against the Python leg
//! (`harnesses/codex.py` `resume`) before that leg was deleted: under
//! `FNO_CAPTURE_GOLDEN=1` the test runs Python, asserts Rust==Python, and
//! freezes the golden under `tests/golden/codex_ask/`; normally it asserts
//! the Rust outcome against that frozen golden and Python never runs.
//!
//! Three cases stay LIVE differential on purpose, because their Python
//! counterparts SURVIVE the port as the codex one-shot spawn substrate:
//! `create`, the no-JSONL exit-11 path, soft-error promotion, and
//! `inject_from_name`. Those keep the skip-when-Python-unavailable policy.
//!
//! Cases covered:
//! - Create happy path (reply text + session_id capture) [live differential]
//! - Resume happy path (reply text, no session_id re-capture) [golden]
//! - Non-zero exit with no JSONL (exit 11 from NoSessionIdError) [live]
//! - Soft error item promotion (exit 0, reply = error message) [live]

use common::{assert_golden as assert_golden_common, capture_mode, Golden};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

mod common;

/// Module-level PATH mutex: every test that mutates the process-global PATH
/// must hold this lock for the duration of its mutation. Cargo runs tests in
/// parallel by default; a per-test (function-scoped `static`) mutex does NOT
/// serialize against other tests in the same file, so a test that installs a
/// fake codex on PATH can race with another that asserts codex absent, with
/// the second test seeing the leaked fake (caught by Gemini review on PR #371,
/// same pattern as the fix in tests/codex_ask_dispatch.rs).
static PATH_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Path to `cli/src` so Python can import the `fno` package.
/// The PATH used when no repo venv exists. The capability probe and every
/// fallback subprocess share it, so they cannot resolve different python3s.
const TEST_PATH: &str = "/usr/bin:/bin:/usr/local/bin";

fn pythonpath() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../cli/src")
}

fn python_executable() -> PathBuf {
    let venv = pythonpath().join("../.venv/bin/python");
    if venv.is_file() {
        venv
    } else {
        PathBuf::from("python3")
    }
}

/// Whether the Python these tests actually invoke can RUN the flow.
///
/// Probes what the flow needs, not merely what imports. `codex.create` reaches
/// `harness_map`, which imports `tomllib` at CALL time, and tomllib is stdlib
/// only from 3.11. A top-level `import fno.agents.harnesses.codex` succeeds on
/// 3.9 and tells you nothing about that, so this guard used to report a capable
/// environment and then watch every test fail inside it.
/// `None` when the test interpreter can run the flow, else WHY it cannot.
///
/// Returns the probe's own stderr rather than a fixed sentence, so the skip
/// line names the real cause. The generic "not available" it used to print
/// described a missing package, while the actual blocker can be a fallback
/// Python without `tomllib`. A skip whose stated reason is not the reason is
/// its own small lie.
fn python_skip_reason() -> Option<String> {
    let probe = Command::new(python_executable())
        .arg("-c")
        .arg("import tomllib; import fno.agents.harnesses.codex")
        .env("PYTHONPATH", pythonpath())
        // Absolute venv paths ignore PATH. The fallback `python3` resolves
        // through the same PATH as every test subprocess.
        .env("PATH", TEST_PATH)
        .output();
    match probe {
        Ok(o) if o.status.success() => None,
        Ok(o) => {
            // The LAST line, which is the exception. A Python traceback leads
            // with "Traceback (most recent call last):" and buries the cause at
            // the bottom, so quoting from the top reports a header instead of a
            // reason - the same trap as reading a log's head for its verdict.
            let err = String::from_utf8_lossy(&o.stderr);
            let cause = err
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("python3 exited nonzero with no stderr")
                .trim()
                .to_string();
            Some(cause)
        }
        Err(e) => Some(format!("could not run python3: {e}")),
    }
}

fn tmpdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "fno-codex-parity-{}-{}-{}",
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

/// Install a fake `codex` binary identical to the one in `codex_ask_dispatch.rs`.
fn install_fake_codex(bin_dir: &Path) {
    let script = r#"#!/bin/sh
set -e
if [ -n "$FAKE_CODEX_EXIT" ] && [ "$FAKE_CODEX_EXIT" != "0" ]; then
  exit "$FAKE_CODEX_EXIT"
fi
if [ -z "$FAKE_CODEX_NO_OUTPUT" ]; then
  if [ -n "$FAKE_CODEX_SESSION_ID" ]; then
    printf '{"type":"thread.started","thread_id":"%s"}\n' "$FAKE_CODEX_SESSION_ID"
  fi
  if [ -n "$FAKE_CODEX_SOFT_ERROR" ]; then
    printf '{"type":"item.completed","item":{"type":"error","message":"%s"}}\n' "$FAKE_CODEX_SOFT_ERROR"
  fi
  if [ -n "$FAKE_CODEX_REPLY" ]; then
    printf '{"type":"item.completed","item":{"type":"agent_message","text":"%s"}}\n' "$FAKE_CODEX_REPLY"
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

/// Drive the Python `harnesses.codex.create` or `.resume` and return `(exit_code, last_msg)`.
///
/// MUST hold the module-level `PATH_MUTEX` for the duration of the call.
/// `cmd.env(...)` adds keys ON TOP of the inherited environment, so if a
/// concurrent `rust_codex_*` is currently mutating the test runner's global
/// env (it sets FAKE_CODEX_* keys before invoking codex_create/resume and
/// removes them after), the python3 subprocess this function forks would
/// inherit those leaked vars and produce the wrong reply text — even though
/// the rust mutators serialize against each other on the same mutex. The
/// fix is to make py_codex serialize on the SAME mutex so no rust mutator
/// can be holding global env state at fork time. Reproduced in CI (PR #371)
/// under high parallelism but not locally; the cleanup-loop / mutex-Drop
/// ordering is correct, the bug is that env inheritance ignores the lock.
fn py_codex(
    mode: &str, // "create" or "resume"
    session_id: Option<&str>,
    cwd: &Path,
    prompt: &str,
    from_name: &str,
    yolo: bool,
    output_path: &Path,
    timeout_sec: u64,
    bin_dir: &Path,
    extra_env: &[(&str, &str)],
) -> (i32, String) {
    let _yolo_val = if yolo { "True" } else { "False" };
    let sess = session_id.unwrap_or("");

    let code = format!(
        r#"
import os, sys
sys.path.insert(0, os.environ["PYTHONPATH"])
from pathlib import Path
from fno.agents.harnesses import codex as c

prompt = os.environ.get("PROMPT","")
from_name = os.environ.get("FROM_NAME","fno")
output_path = Path(os.environ["OUTPUT_PATH"])
cwd = Path(os.environ["CWD"])
session_id = os.environ.get("SESSION_ID","")
timeout = float(os.environ.get("TIMEOUT","10"))
yolo = os.environ.get("YOLO","0") == "1"

try:
    if "{mode}" == "create":
        r = c.create(cwd=cwd, prompt=prompt, from_name=from_name, yolo=yolo, output_path=output_path, timeout=timeout)
    else:
        r = c.resume(session_id=session_id, cwd=cwd, prompt=prompt, from_name=from_name, yolo=yolo, output_path=output_path, timeout=timeout)
    sys.stdout.write(r.last_msg or "")
    sys.exit(r.exit_code)
except c.NoSessionIdError:
    sys.exit(11)
except c.CodexTimeoutError:
    sys.exit(15)
except c.CodexInvocationError as e:
    sys.exit(e.exit_code if e.exit_code != 0 else 1)
"#,
        mode = mode
    );

    // Serialize against rust_codex_create / rust_codex_resume which mutate
    // the test runner's global env. Subprocess env inheritance happens at
    // fork time and ignores Rust-level mutex protection by default, so
    // without this lock python3 could see a sibling test's leaked FAKE_*
    // vars between that sibling's env-set and env-remove. The lock is
    // held for the full subprocess lifetime: cmd.output() blocks until
    // python3 exits, so any concurrent rust_codex_* mutation cannot
    // interleave with the fork.
    let _guard = PATH_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

    let mut cmd = Command::new(python_executable());
    cmd.arg("-c").arg(&code);
    // Defense in depth: explicitly clear every FAKE_CODEX_* var that any
    // sibling test might set. If global env IS dirty at fork time despite
    // the mutex (e.g., a future test that bypasses the lock), env_remove
    // strips the leaked value from the cmd's resolved environment before
    // fork. List is the union of keys set across all tests in this file.
    for k in [
        "FAKE_CODEX_SESSION_ID",
        "FAKE_CODEX_REPLY",
        "FAKE_CODEX_SOFT_ERROR",
        "FAKE_CODEX_EXIT",
        "FAKE_CODEX_TURN_COMPLETE",
        "FAKE_CODEX_NO_OUTPUT",
    ] {
        cmd.env_remove(k);
    }
    cmd.env("PYTHONPATH", pythonpath());
    cmd.env("PROMPT", prompt);
    cmd.env("FROM_NAME", from_name);
    cmd.env("OUTPUT_PATH", output_path);
    cmd.env("CWD", cwd);
    cmd.env("SESSION_ID", sess);
    cmd.env("TIMEOUT", timeout_sec.to_string());
    cmd.env("YOLO", if yolo { "1" } else { "0" });
    // Put fake codex on PATH.
    let _old_path = std::env::var_os("PATH").unwrap_or_default();
    let new_path = format!("{}:/usr/bin:/bin:/usr/local/bin", bin_dir.display());
    cmd.env("PATH", &new_path);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }

    let out = cmd.output().expect("run python3");
    let exit_code = out.status.code().unwrap_or(1);
    let last_msg = String::from_utf8_lossy(&out.stdout).to_string();
    // On a nonzero exit, fold stderr into the returned string. `cmd.output()`
    // captures the Python traceback that EXPLAINS the exit, and discarding it
    // left every failure here reading `left: 1, right: 0` with no cause. The
    // diagnostic was in hand and the instrument threw it away.
    if exit_code != 0 {
        let err = String::from_utf8_lossy(&out.stderr).to_string();
        return (
            exit_code,
            format!("{last_msg}\n--- python stderr ---\n{err}"),
        );
    }
    (exit_code, last_msg)
}

/// Drive the Rust codex_create with the fake codex binary on PATH.
fn rust_codex_create(
    cwd: &Path,
    prompt: &str,
    from_name: &str,
    yolo: bool,
    output_path: &Path,
    timeout_sec: u64,
    bin_dir: &Path,
    extra_env: &[(&str, &str)],
) -> (i32, String) {
    use fno_agents::codex_ask::codex_create;

    let _guard = PATH_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

    let old_path = std::env::var_os("PATH");
    let new_path = format!("{}:/usr/bin:/bin:/usr/local/bin", bin_dir.display());
    unsafe { std::env::set_var("PATH", &new_path) };
    for (k, v) in extra_env {
        unsafe { std::env::set_var(k, v) };
    }

    let result = codex_create(
        cwd,
        prompt,
        from_name,
        yolo,
        output_path,
        Some(std::time::Duration::from_secs(timeout_sec)),
        None,
        None,
        None,
        None,
    );

    match old_path {
        Some(p) => unsafe { std::env::set_var("PATH", p) },
        None => unsafe { std::env::remove_var("PATH") },
    }
    for (k, _) in extra_env {
        unsafe { std::env::remove_var(k) };
    }

    match result {
        Ok(r) => (r.exit_code, r.last_msg),
        Err(e) => (e.exit_code(), String::new()),
    }
}

/// Drive the Rust codex_resume with the fake codex binary on PATH.
fn rust_codex_resume(
    session_id: &str,
    cwd: &Path,
    prompt: &str,
    from_name: &str,
    yolo: bool,
    output_path: &Path,
    timeout_sec: u64,
    bin_dir: &Path,
    extra_env: &[(&str, &str)],
) -> (i32, String) {
    use fno_agents::codex_ask::codex_resume;

    let _guard = PATH_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

    let old_path = std::env::var_os("PATH");
    let new_path = format!("{}:/usr/bin:/bin:/usr/local/bin", bin_dir.display());
    unsafe { std::env::set_var("PATH", &new_path) };
    for (k, v) in extra_env {
        unsafe { std::env::set_var(k, v) };
    }

    let result = codex_resume(
        session_id,
        cwd,
        prompt,
        from_name,
        yolo,
        output_path,
        Some(std::time::Duration::from_secs(timeout_sec)),
        None,
        None,
    );

    match old_path {
        Some(p) => unsafe { std::env::set_var("PATH", p) },
        None => unsafe { std::env::remove_var("PATH") },
    }
    for (k, _) in extra_env {
        unsafe { std::env::remove_var(k) };
    }

    match result {
        Ok(r) => (r.exit_code, r.last_msg),
        Err(e) => (e.exit_code(), String::new()),
    }
}

// ---------------------------------------------------------------------------
// Parity case 1: create happy path
// ---------------------------------------------------------------------------

#[test]
fn parity_create_happy_path() {
    if let Some(why) = python_skip_reason() {
        eprintln!("SKIP: the test interpreter cannot run this flow: {why}");
        return;
    }

    let bin_dir = tmpdir("par-c1-bin");
    let cwd = tmpdir("par-c1-cwd");
    install_fake_codex(&bin_dir);

    let session_id = "par11111-1111-2222-3333-444455556666";
    let reply = "parity create reply";
    let extra = [
        ("FAKE_CODEX_SESSION_ID", session_id),
        ("FAKE_CODEX_REPLY", reply),
    ];

    let py_out = tmpdir("par-c1-py-out").join("out.jsonl");
    let rs_out = tmpdir("par-c1-rs-out").join("out.jsonl");

    let (py_exit, py_reply) = py_codex(
        "create", None, &cwd, "hello", "fno", false, &py_out, 10, &bin_dir, &extra,
    );
    let (rs_exit, rs_reply) =
        rust_codex_create(&cwd, "hello", "fno", false, &rs_out, 10, &bin_dir, &extra);

    assert_eq!(py_exit, 0, "Python create exit: {}\n{}", py_exit, py_reply);
    assert_eq!(rs_exit, 0, "Rust create exit: {}", rs_exit);
    assert_eq!(
        py_reply, rs_reply,
        "Reply mismatch: python={:?} rust={:?}",
        py_reply, rs_reply
    );
    assert_eq!(py_reply, reply, "Expected reply text: {}", reply);
}

// ---------------------------------------------------------------------------
// Parity case 2: resume happy path
// ---------------------------------------------------------------------------

#[test]
fn parity_resume_happy_path() {
    // Capture mode needs the Python leg to freeze against; the golden-read
    // path needs nothing but the fake codex binary.
    if capture_mode() {
        if let Some(why) = python_skip_reason() {
            eprintln!("SKIP: the test interpreter cannot run this flow: {why}");
            return;
        }
    }

    let bin_dir = tmpdir("par-r1-bin");
    let cwd = tmpdir("par-r1-cwd");
    install_fake_codex(&bin_dir);

    let session_id = "par22222-1111-2222-3333-444455556666";
    let reply = "parity resume reply";
    let extra = [("FAKE_CODEX_REPLY", reply)]; // No FAKE_CODEX_SESSION_ID on resume

    let rs_out = tmpdir("par-r1-rs-out").join("out.jsonl");

    let (rs_exit, rs_reply) = rust_codex_resume(
        session_id,
        &cwd,
        "follow up",
        "fno",
        false,
        &rs_out,
        10,
        &bin_dir,
        &extra,
    );
    let rust = Golden {
        exit: Some(rs_exit),
        streams: vec![rs_reply],
    };

    // Capture mode freezes the Python resume outcome as the golden (protocol
    // step 2): resume is the codex ASK leg, the one case here whose Python
    // implementation is deleted by the port. Normal mode (the deleted-Python
    // world) reads the frozen golden and asserts Rust matches it.
    let oracle = capture_mode().then(|| {
        let py_out = tmpdir("par-r1-py-out").join("out.jsonl");
        let (py_exit, py_reply) = py_codex(
            "resume",
            Some(session_id),
            &cwd,
            "follow up",
            "fno",
            false,
            &py_out,
            10,
            &bin_dir,
            &extra,
        );
        Golden {
            exit: Some(py_exit),
            streams: vec![py_reply],
        }
    });
    assert_golden_common("codex_ask", "resume happy path", &rust, oracle);
}

// ---------------------------------------------------------------------------
// Parity case 3: non-zero exit with no JSONL -> NoSessionIdError exit 11
// ---------------------------------------------------------------------------

#[test]
fn parity_no_jsonl_nonzero_exit() {
    if let Some(why) = python_skip_reason() {
        eprintln!("SKIP: the test interpreter cannot run this flow: {why}");
        return;
    }

    let bin_dir = tmpdir("par-e1-bin");
    let cwd = tmpdir("par-e1-cwd");
    install_fake_codex(&bin_dir);

    let extra = [("FAKE_CODEX_EXIT", "3")]; // exit 3, no JSONL

    let py_out = tmpdir("par-e1-py-out").join("out.jsonl");
    let rs_out = tmpdir("par-e1-rs-out").join("out.jsonl");

    let (py_exit, _) = py_codex(
        "create", None, &cwd, "hello", "fno", false, &py_out, 10, &bin_dir, &extra,
    );
    let (rs_exit, _) =
        rust_codex_create(&cwd, "hello", "fno", false, &rs_out, 10, &bin_dir, &extra);

    // Both should exit 11 (NoSessionIdError fires before exit_code check in Python)
    assert_eq!(
        py_exit, 11,
        "Python no-JSONL exit should be 11, got {}",
        py_exit
    );
    assert_eq!(
        rs_exit, 11,
        "Rust no-JSONL exit should be 11, got {}",
        rs_exit
    );
}

// ---------------------------------------------------------------------------
// Parity case 4: soft error promotion (exit 0, reply = error message)
// ---------------------------------------------------------------------------

#[test]
fn parity_soft_error_promotion() {
    if let Some(why) = python_skip_reason() {
        eprintln!("SKIP: the test interpreter cannot run this flow: {why}");
        return;
    }

    let bin_dir = tmpdir("par-se1-bin");
    let cwd = tmpdir("par-se1-cwd");
    install_fake_codex(&bin_dir);

    let session_id = "par33333-1111-2222-3333-444455556666";
    let soft_err = "hooks deprecated warning";
    let extra = [
        ("FAKE_CODEX_SESSION_ID", session_id),
        ("FAKE_CODEX_SOFT_ERROR", soft_err),
        // No FAKE_CODEX_REPLY
    ];

    let py_out = tmpdir("par-se1-py-out").join("out.jsonl");
    let rs_out = tmpdir("par-se1-rs-out").join("out.jsonl");

    let (py_exit, py_reply) = py_codex(
        "create", None, &cwd, "hello", "fno", false, &py_out, 10, &bin_dir, &extra,
    );
    let (rs_exit, rs_reply) =
        rust_codex_create(&cwd, "hello", "fno", false, &rs_out, 10, &bin_dir, &extra);

    assert_eq!(
        py_exit, 0,
        "Python soft-error exit: {}\n{}",
        py_exit, py_reply
    );
    assert_eq!(rs_exit, 0, "Rust soft-error exit: {}", rs_exit);
    assert_eq!(
        py_reply, rs_reply,
        "Soft error reply mismatch: python={:?} rust={:?}",
        py_reply, rs_reply
    );
    assert_eq!(
        py_reply, soft_err,
        "Expected soft error text promoted to reply"
    );
}

// ---------------------------------------------------------------------------
// Parity case 5: inject_from_name parity
// ---------------------------------------------------------------------------

#[test]
fn parity_inject_from_name() {
    if let Some(why) = python_skip_reason() {
        eprintln!("SKIP: the test interpreter cannot run this flow: {why}");
        return;
    }

    let code = r#"
import os, sys
sys.path.insert(0, os.environ["PYTHONPATH"])
from fno.agents.harnesses.codex import inject_from_name
prompt = os.environ.get("PROMPT","")
from_name = os.environ.get("FROM_NAME","fno")
sys.stdout.write(inject_from_name(prompt, from_name))
"#;

    let cases = vec![
        ("hello world", "alice"),
        ("", "x"),
        ("multi\nline\nprompt", "agent-a"),
        ("a&b<c>", "x\"y"),
    ];

    for (prompt, from_name) in cases {
        let py_out = Command::new(python_executable())
            .arg("-c")
            .arg(code)
            .env("PYTHONPATH", pythonpath())
            .env("PATH", TEST_PATH)
            .env("PROMPT", prompt)
            .env("FROM_NAME", from_name)
            .output()
            .expect("python inject_from_name");
        assert!(
            py_out.status.success(),
            "python failed for {:?}/{:?}",
            prompt,
            from_name
        );
        let py_result = String::from_utf8_lossy(&py_out.stdout).to_string();
        let rs_result = fno_agents::codex_ask::inject_from_name(prompt, from_name);
        assert_eq!(
            py_result, rs_result,
            "inject_from_name mismatch for ({:?}, {:?}): python={:?} rust={:?}",
            prompt, from_name, py_result, rs_result
        );
    }
}
