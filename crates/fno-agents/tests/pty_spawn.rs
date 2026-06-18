//! Real-subprocess PTY tests (Discretion #5: real `Command` spawn, never
//! monkeypatched). These spawn actual `bash` children on real PTYs and assert
//! the drainer captures output and input round-trips, mirroring the US4-codex /
//! US4-gemini real-subprocess discipline.

use fno_agents::pty::{DrainOutcome, PtySession, DEFAULT_OUTPUT_RING_BYTES};
use portable_pty::CommandBuilder;
use std::time::{Duration, Instant};

/// Poll the session snapshot until `needle` appears or `timeout` elapses.
fn wait_for_output(session: &PtySession, needle: &str, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        let snap = session.snapshot();
        if String::from_utf8_lossy(&snap).contains(needle) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

#[test]
fn spawns_child_and_drains_output() {
    let mut cmd = CommandBuilder::new("bash");
    cmd.arg("-c");
    cmd.arg("printf 'PTY_SUBSTRATE_OK\\n'");

    let session =
        PtySession::spawn(cmd, 24, 80, DEFAULT_OUTPUT_RING_BYTES).expect("spawn should succeed");
    assert!(
        session.child_pid().is_some(),
        "child pid should be reported"
    );

    assert!(
        wait_for_output(&session, "PTY_SUBSTRATE_OK", Duration::from_secs(5)),
        "drainer should capture the child's stdout; got: {:?}",
        String::from_utf8_lossy(&session.snapshot())
    );

    // The child exits on its own; wait reaps it.
    let _ = session.wait();
    assert!(!session.is_child_alive(), "child should have exited");

    // After a clean child exit the drainer should record EOF, not an error -
    // this is the signal a daemon uses to tell a healthy exit from a PTY fault.
    let start = Instant::now();
    loop {
        match session.drain_outcome() {
            DrainOutcome::Eof => break,
            DrainOutcome::Errored { kind, message } => {
                panic!("clean exit should be EOF, got error: {kind} / {message}")
            }
            DrainOutcome::Running => {
                if start.elapsed() > Duration::from_secs(3) {
                    panic!("drainer never recorded a terminal outcome");
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

#[test]
fn input_round_trips_through_pty_echo() {
    // `cat` on a PTY in canonical mode echoes input; writing a line should
    // surface it in the drained output.
    let cmd = CommandBuilder::new("cat");
    let session =
        PtySession::spawn(cmd, 24, 80, DEFAULT_OUTPUT_RING_BYTES).expect("spawn should succeed");

    session
        .write_input(b"roundtrip-marker-7be3\n")
        .expect("write should succeed");

    assert!(
        wait_for_output(&session, "roundtrip-marker-7be3", Duration::from_secs(5)),
        "input should round-trip via PTY echo; got: {:?}",
        String::from_utf8_lossy(&session.snapshot())
    );

    // cat does not exit on its own; kill it and confirm liveness flips.
    assert!(session.is_child_alive(), "cat should still be running");
    session.kill().expect("kill should succeed");
    let _ = session.wait();
    assert!(!session.is_child_alive(), "child should be dead after kill");
}

#[test]
fn resize_does_not_error_on_live_pty() {
    let cmd = CommandBuilder::new("cat");
    let session =
        PtySession::spawn(cmd, 24, 80, DEFAULT_OUTPUT_RING_BYTES).expect("spawn should succeed");
    session
        .resize(40, 120)
        .expect("resize should succeed on a live pty");
    session.kill().expect("kill should succeed");
    let _ = session.wait();
}

/// `worker::run` stamps `FNO_AGENTS_SELF_SHORT_ID` onto the PTY child's
/// environment via `CommandBuilder::env` (cv-140f09c3) so the drive-authority
/// guard can scope to THIS agent's identity. LD3 only refuses an operator-typed
/// `<promise>` if that identity actually reaches the bash Stop / graph-write-
/// protect hook the PTY child spawns - and the child (claude/codex) runs those
/// hooks as its OWN subprocesses, which inherit its environment. So the var must
/// survive two hops: worker -> PTY child -> hook.
///
/// This test proves the in-our-code hops against the REAL spawn path (the same
/// `CommandBuilder` + `PtySession::spawn` `worker::run` uses): a var set via
/// `CommandBuilder::env` reaches a GRANDCHILD process the PTY child forks - the
/// hook's vantage point - not merely the child shell. The final real-world hop
/// (claude forwarding its env to the hook subprocess it spawns) is the Claude
/// Code harness's standard subprocess-env inheritance, exercised here by the
/// grandchild bash. Verifies ab-1e86b88e (handoff CRITICAL step 3, never
/// previously asserted); since propagation holds, no fallback identity source is
/// needed.
#[test]
fn self_short_id_env_reaches_a_grandchild_of_the_pty_child() {
    let mut cmd = CommandBuilder::new("bash");
    cmd.arg("-c");
    // Outer bash = the PTY child (claude/codex). It spawns an INNER bash as a
    // SEPARATE process = the Stop / graph-write-protect hook. The inner process
    // reads the var from its inherited environment and prints it, so a hit proves
    // propagation across the worker -> child -> hook process boundary, not mere
    // same-shell visibility.
    cmd.arg(r#"bash -c 'printf "HOOK_SEES=%s\n" "$FNO_AGENTS_SELF_SHORT_ID"'"#);
    // The exact stamp worker::run performs (crates/fno-agents/src/worker.rs).
    cmd.env("FNO_AGENTS_SELF_SHORT_ID", "wk-propagation-7c1d");

    let session =
        PtySession::spawn(cmd, 24, 80, DEFAULT_OUTPUT_RING_BYTES).expect("spawn should succeed");

    assert!(
        wait_for_output(
            &session,
            "HOOK_SEES=wk-propagation-7c1d",
            Duration::from_secs(5)
        ),
        "FNO_AGENTS_SELF_SHORT_ID must reach a grandchild of the PTY child (the hook's vantage); got: {:?}",
        String::from_utf8_lossy(&session.snapshot())
    );

    // Both bash processes exit on their own; wait reaps the PTY child.
    let _ = session.wait();
    assert!(!session.is_child_alive(), "child should have exited");
}
