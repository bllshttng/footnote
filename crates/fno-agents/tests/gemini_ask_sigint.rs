//! Isolated end-to-end test for SIGINT forwarding on the gemini path
//! (cv-cfdb7a56, via the shared `subprocess_ask` driver; ab-73da4ac2).
//!
//! Mirror of `codex_ask_sigint.rs`. Self-delivers SIGINT to the test PROCESS
//! to exercise the process-global SIGINT-forwarding handler installed by
//! `run_gemini` (now `subprocess_ask::SigintForwarder`). Kept as the sole pair
//! of tests in its own binary so the signal can only land while the single
//! gemini call is mid-flight.

// Task 1.3a: `ask` never creates, so these tests drive the retained create
// machinery through `dispatch_gemini_once` (spawn --once), which is the path
// that installs the SIGINT-forwarding handler around `run_gemini`.
use fno_agents::gemini_ask::dispatch_gemini_once;
use fno_agents::paths::AgentsHome;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Both tests mutate process-global SIGINT disposition; serialize them.
static SIGNAL_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn tmpdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "abi-gemini-sigint-{}-{}-{}",
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

/// Fake `gemini` that traps SIGINT/SIGTERM, writes "caught" to the sentinel,
/// and exits 130. It sleeps long enough that the forwarded signal lands while
/// the Rust client is blocked reading the (not-yet-emitted) stdout blob.
fn install_fake_gemini_sigint(bin_dir: &Path) {
    let script = r#"#!/bin/sh
trap 'printf caught > "$FAKE_GEMINI_SIGINT_SENTINEL"; exit 130' INT TERM
sleep "${FAKE_GEMINI_SLEEP:-10}"
printf '{"session_id":"%s","response":"done","stats":{}}' "$FAKE_GEMINI_SESSION_ID"
"#;
    let path = bin_dir.join("gemini");
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// cv-cfdb7a56 (shared driver): an operator Ctrl-C must be FORWARDED to the
/// gemini process group (so gemini + descendants tear down instead of
/// orphaning), and the dispatch must report exit 130. The fake traps the
/// forwarded signal and writes a sentinel; its existence proves the relay.
#[test]
fn gemini_create_ctrl_c_forwards_sigint_and_exits_130() {
    let _serial = SIGNAL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let home = AgentsHome::at(tmpdir("home"));
    let bin_dir = tmpdir("bin");
    let cwd = tmpdir("cwd");
    install_fake_gemini_sigint(&bin_dir);

    let sentinel = tmpdir("sentinel").join("caught.txt");
    let sentinel_str = sentinel.to_string_lossy().to_string();

    let old_path = std::env::var_os("PATH");
    let new_path = format!("{}:/usr/bin:/bin:/usr/local/bin", bin_dir.display());
    unsafe { std::env::set_var("PATH", &new_path) };
    unsafe {
        std::env::set_var(
            "FAKE_GEMINI_SESSION_ID",
            "geeeffff-1111-2222-3333-444455556666",
        )
    };
    unsafe { std::env::set_var("FAKE_GEMINI_SLEEP", "10") };
    unsafe { std::env::set_var("FAKE_GEMINI_SIGINT_SENTINEL", &sentinel_str) };

    let signal_thread = std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(1000));
        // SAFETY: kill(2) with our own pid is async-signal-safe and standard.
        unsafe {
            libc::kill(libc::getpid(), libc::SIGINT);
        }
    });

    let outcome = dispatch_gemini_once(
        &home,
        "sigint-gemini",
        "hi",
        "abilities",
        &cwd,
        false,
        Some(Duration::from_secs(30)), // large timeout: the interrupt wins
        None,
    );

    let _ = signal_thread.join();

    match old_path {
        Some(p) => unsafe { std::env::set_var("PATH", p) },
        None => unsafe { std::env::remove_var("PATH") },
    }
    unsafe { std::env::remove_var("FAKE_GEMINI_SESSION_ID") };
    unsafe { std::env::remove_var("FAKE_GEMINI_SLEEP") };
    unsafe { std::env::remove_var("FAKE_GEMINI_SIGINT_SENTINEL") };

    assert_eq!(
        outcome.exit_code, 130,
        "operator Ctrl-C should exit 130: {} (stderr: {})",
        outcome.exit_code, outcome.stderr
    );
    assert!(
        sentinel.exists(),
        "SIGINT must be forwarded to the gemini process group (sentinel not written) — gemini would be orphaned"
    );
}

/// With a SIG_IGN parent (fno backgrounded / under a supervisor), the forwarder
/// must HONOR that: not install its handler, not forward, and leave the
/// disposition SIG_IGN on drop. Run a fast normal gemini create and assert the
/// post-run disposition is still SIG_IGN.
#[test]
fn sigint_ignored_parent_disposition_is_preserved() {
    let _serial = SIGNAL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let home = AgentsHome::at(tmpdir("ign-home"));
    let bin_dir = tmpdir("ign-bin");
    let cwd = tmpdir("ign-cwd");
    install_fake_gemini_sigint(&bin_dir);

    // SAFETY: standard libc signal calls, serialized by SIGNAL_TEST_LOCK.
    let original = unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };

    let old_path = std::env::var_os("PATH");
    let new_path = format!("{}:/usr/bin:/bin:/usr/local/bin", bin_dir.display());
    unsafe { std::env::set_var("PATH", &new_path) };
    unsafe {
        std::env::set_var(
            "FAKE_GEMINI_SESSION_ID",
            "g1112222-3333-4444-5555-666677778888",
        )
    };
    unsafe { std::env::set_var("FAKE_GEMINI_SLEEP", "0") };

    let outcome = dispatch_gemini_once(
        &home,
        "ign-gemini",
        "hi",
        "abilities",
        &cwd,
        false,
        Some(Duration::from_secs(10)),
        None,
    );

    let after = unsafe { libc::signal(libc::SIGINT, libc::SIG_DFL) };
    unsafe { libc::signal(libc::SIGINT, original) };

    match old_path {
        Some(p) => unsafe { std::env::set_var("PATH", p) },
        None => unsafe { std::env::remove_var("PATH") },
    }
    unsafe { std::env::remove_var("FAKE_GEMINI_SESSION_ID") };
    unsafe { std::env::remove_var("FAKE_GEMINI_SLEEP") };

    assert_eq!(
        outcome.exit_code, 0,
        "normal run should succeed: {}",
        outcome.stderr
    );
    assert_eq!(
        after,
        libc::SIG_IGN,
        "with a SIG_IGN parent, the forwarder must leave SIGINT as SIG_IGN"
    );
}
