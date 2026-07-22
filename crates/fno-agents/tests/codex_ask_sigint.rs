//! Isolated end-to-end test for SIGINT forwarding (ab-e7fdbcb6 / cv-cfdb7a56).
//!
//! This test self-delivers SIGINT to the test PROCESS (`kill(getpid(), SIGINT)`)
//! to exercise the process-global SIGINT-forwarding handler installed by
//! `run_codex`. Because the handler is process-global and installed only for
//! the lifetime of one `run_codex` call, a signal that lands while NO handler
//! is installed would hit the default disposition and abort the harness.
//!
//! Keeping this as the SOLE test in its own binary makes the scenario
//! deterministic: nothing else runs in this process, so the only window in
//! which the signal can land is while the single `run_codex` call is mid-flight
//! (the fake codex sleeps 10s; we signal at 1s). `codex_ask_dispatch.rs` keeps
//! the rest of the hardening tests, which never touch process signals.

// Task 1.3a: `ask` never creates, so these tests drive the retained create
// machinery through `dispatch_codex_once` (spawn --once), which is the path
// that installs the SIGINT-forwarding handler around `run_codex`.
use fno_agents::codex_ask::dispatch_codex_once;
use fno_agents::paths::AgentsHome;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Both tests in this binary mutate process-global SIGINT disposition, so they
/// must not run concurrently. cargo runs tests in a binary on parallel threads
/// by default; this lock serializes them within the process.
static SIGNAL_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn tmpdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "fno-codex-sigint-{}-{}-{}",
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

/// Fake `codex` that traps SIGINT/SIGTERM, writes "caught" to the sentinel path,
/// and exits 130. It emits a session + reply, then sleeps long enough that the
/// test's forwarded signal lands mid-turn (before turn.completed).
fn install_fake_codex_sigint(bin_dir: &Path) {
    let script = r#"#!/bin/sh
set -e
trap 'printf caught > "$FAKE_CODEX_SIGINT_SENTINEL"; exit 130' INT TERM
printf '{"type":"thread.started","thread_id":"%s"}\n' "$FAKE_CODEX_SESSION_ID"
printf '{"type":"item.completed","item":{"type":"agent_message","text":"%s"}}\n' "$FAKE_CODEX_REPLY"
sleep "${FAKE_CODEX_SLEEP:-10}"
printf '{"type":"turn.completed"}\n'
"#;
    let path = bin_dir.join("codex");
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// ab-e7fdbcb6 (cv-cfdb7a56): an operator Ctrl-C must be FORWARDED to the codex
/// process group (so codex + its sandbox tear down instead of orphaning), and
/// the dispatch must report exit 130. The fake codex traps the forwarded signal
/// and writes a sentinel file; its existence proves the signal was relayed to
/// the codex group rather than only killing fno.
#[test]
fn codex_create_ctrl_c_forwards_sigint_and_exits_130() {
    let _serial = SIGNAL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let home = AgentsHome::at(tmpdir("home"));
    let bin_dir = tmpdir("bin");
    let cwd = tmpdir("cwd");
    install_fake_codex_sigint(&bin_dir);

    let sentinel = tmpdir("sentinel").join("caught.txt");
    let sentinel_str = sentinel.to_string_lossy().to_string();

    let old_path = std::env::var_os("PATH");
    let new_path = format!("{}:/usr/bin:/bin:/usr/local/bin", bin_dir.display());
    unsafe { std::env::set_var("PATH", &new_path) };
    unsafe {
        std::env::set_var(
            "FAKE_CODEX_SESSION_ID",
            "eeeeffff-1111-2222-3333-444455556666",
        )
    };
    unsafe { std::env::set_var("FAKE_CODEX_REPLY", "interrupt me") };
    unsafe { std::env::set_var("FAKE_CODEX_SLEEP", "10") }; // sleep long; we interrupt
    unsafe { std::env::set_var("FAKE_CODEX_SIGINT_SENTINEL", &sentinel_str) };

    // Deliver SIGINT to ourselves ~1s in, while the single run_codex call (and
    // thus the SIGINT-forwarding handler) is mid-flight on the 10s codex sleep.
    let signal_thread = std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(1000));
        // SAFETY: kill(2) with our own pid is async-signal-safe and standard.
        unsafe {
            libc::kill(libc::getpid(), libc::SIGINT);
        }
    });

    let outcome = dispatch_codex_once(
        &home,
        "sigint-agent",
        "hi",
        "fno",
        &cwd,
        false,
        Some(Duration::from_secs(30)), // large timeout: the interrupt wins
        None,
        None,
        None,
    );

    let _ = signal_thread.join();

    match old_path {
        Some(p) => unsafe { std::env::set_var("PATH", p) },
        None => unsafe { std::env::remove_var("PATH") },
    }
    unsafe { std::env::remove_var("FAKE_CODEX_SESSION_ID") };
    unsafe { std::env::remove_var("FAKE_CODEX_REPLY") };
    unsafe { std::env::remove_var("FAKE_CODEX_SLEEP") };
    unsafe { std::env::remove_var("FAKE_CODEX_SIGINT_SENTINEL") };

    assert_eq!(
        outcome.exit_code, 130,
        "operator Ctrl-C should exit 130: {} (stderr: {})",
        outcome.exit_code, outcome.stderr
    );
    assert!(
        sentinel.exists(),
        "SIGINT must be forwarded to the codex process group (sentinel not written) — codex would be orphaned"
    );
}

/// Gemini PR #372 HIGH (2): if the parent process has SIGINT set to SIG_IGN
/// (e.g. fno runs backgrounded or under a supervisor), the forwarder must HONOR
/// that — not install its handler, not forward — and must leave the disposition
/// as SIG_IGN when it drops. Here we set SIG_IGN, run a fast normal codex
/// create, and assert the post-run disposition is still SIG_IGN (proving our
/// handler was undone and the guard restored the parent's choice).
#[test]
fn sigint_ignored_parent_disposition_is_preserved() {
    let _serial = SIGNAL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let home = AgentsHome::at(tmpdir("ign-home"));
    let bin_dir = tmpdir("ign-bin");
    let cwd = tmpdir("ign-cwd");
    // Fast fake codex: no sleep, completes immediately.
    install_fake_codex_sigint(&bin_dir);

    // Save the real disposition, then set SIG_IGN as the "parent" state.
    // SAFETY: standard libc signal calls, serialized by SIGNAL_TEST_LOCK.
    let original = unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };

    let old_path = std::env::var_os("PATH");
    let new_path = format!("{}:/usr/bin:/bin:/usr/local/bin", bin_dir.display());
    unsafe { std::env::set_var("PATH", &new_path) };
    unsafe {
        std::env::set_var(
            "FAKE_CODEX_SESSION_ID",
            "11112222-3333-4444-5555-666677778888",
        )
    };
    unsafe { std::env::set_var("FAKE_CODEX_REPLY", "quick reply") };
    unsafe { std::env::set_var("FAKE_CODEX_SLEEP", "0") };
    // No sentinel: this run completes normally, no signal involved.

    let outcome = dispatch_codex_once(
        &home,
        "ign-agent",
        "hi",
        "fno",
        &cwd,
        false,
        Some(Duration::from_secs(10)),
        None,
        None,
        None,
    );

    // Read back the disposition by installing SIG_DFL and capturing what was there.
    let after = unsafe { libc::signal(libc::SIGINT, libc::SIG_DFL) };
    // Restore the real original disposition.
    unsafe { libc::signal(libc::SIGINT, original) };

    match old_path {
        Some(p) => unsafe { std::env::set_var("PATH", p) },
        None => unsafe { std::env::remove_var("PATH") },
    }
    unsafe { std::env::remove_var("FAKE_CODEX_SESSION_ID") };
    unsafe { std::env::remove_var("FAKE_CODEX_REPLY") };
    unsafe { std::env::remove_var("FAKE_CODEX_SLEEP") };

    assert_eq!(
        outcome.exit_code, 0,
        "normal run should succeed: {}",
        outcome.stderr
    );
    assert_eq!(
        after,
        libc::SIG_IGN,
        "with a SIG_IGN parent, the forwarder must leave SIGINT as SIG_IGN (not its handler, not SIG_DFL)"
    );
}
