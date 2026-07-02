//! Persistence + race tests (task 1.4): scripted versions of the Phase-1
//! manual exit criteria. Detach/reattach exactness, kill -9 survival, dead-
//! server respawn, the two-client cold-start race, and the client-side
//! version-skew refusal - all through the client-under-portable-pty harness.

mod common;

use std::time::Duration;

use common::{ClientHarness, Scratch};
use fno::proto::{read_msg_sync, write_msg_sync, ClientMsg, ServerMsg};

/// Count live processes whose command line carries this scratch's socket
/// path: exactly the servers of this test's session (the path is unique per
/// test, so no cross-talk).
fn server_count(scratch: &Scratch) -> usize {
    let out = std::process::Command::new("pgrep")
        .arg("-f")
        .arg(scratch.main_sock().to_str().unwrap())
        .output()
        .unwrap();
    // pgrep exits 1 with empty output when nothing matches.
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
}

fn kill_server(scratch: &Scratch) {
    let _ = std::process::Command::new("pkill")
        .arg("-9")
        .arg("-f")
        .arg(scratch.main_sock().to_str().unwrap())
        .status();
}

#[test]
fn persistence_reattach_restores_the_exact_screen() {
    // AC3-HP + AC3-UI: reattach lands on the SAME live PTY and the full
    // screen redraws from the server grid - the new client's settled screen
    // is byte-identical to what the old client saw at detach.
    let scratch = Scratch::new("exact");
    let mut h = ClientHarness::spawn(&scratch);
    h.wait_screen(15, |s| !s.trim().is_empty());
    h.type_bytes(b"echo marker-one; echo marker-two\r");
    let before = h.wait_screen(15, |s| s.lines().any(|l| l.trim() == "marker-two"));
    std::thread::sleep(Duration::from_millis(300));
    let before = {
        // Re-read after a settle so the prompt after the command is included.
        let s = h.screen();
        if s != before {
            s
        } else {
            before
        }
    };
    h.type_bytes(&[0x1C]);
    assert!(h.wait_exit(10).success());
    drop(h);

    let mut h2 = ClientHarness::spawn(&scratch);
    h2.wait_screen(15, |s| s == before);
}

#[test]
fn persistence_alt_screen_program_survives_detach_reattach() {
    // AC3-EDGE: a full-screen alt-screen program (vim's mechanism: DECSET
    // 1049) is running at detach; on reattach its screen renders correctly
    // because the SERVER holds the alt-screen grid. Driven with printf+cat so
    // the test needs no vim binary, only the exact escape sequence vim emits.
    let scratch = Scratch::new("altscreen");
    let mut h = ClientHarness::spawn(&scratch);
    h.wait_screen(15, |s| !s.trim().is_empty());
    // Enter alt screen, clear, home, draw a marker; cat holds the program
    // "open" in the foreground exactly like an editor session.
    h.type_bytes(b"printf '\\033[?1049h\\033[2J\\033[HALT-SCREEN-HELD'; cat\r");
    h.wait_screen(15, |s| s.contains("ALT-SCREEN-HELD"));
    h.type_bytes(&[0x1C]); // detach with the alt screen active
    assert!(h.wait_exit(10).success());
    drop(h);

    let mut h2 = ClientHarness::spawn(&scratch);
    let screen = h2.wait_screen(15, |s| s.contains("ALT-SCREEN-HELD"));
    // The alt screen starts with the marker at home - no garbled partial.
    assert!(
        screen.trim_start().starts_with("ALT-SCREEN-HELD"),
        "alt screen must redraw from the top: {screen:?}"
    );
    // Leave the program: ^C ends cat; the shell must still be there.
    h2.type_bytes(&[0x03]);
    h2.type_bytes(b"printf '\\033[?1049l'; echo back-on-main\r");
    h2.wait_screen(15, |s| s.lines().any(|l| l.trim() == "back-on-main"));
}

#[test]
fn persistence_kill_nine_of_the_client_leaves_the_pty_running() {
    // AC4-HP / exit criterion 3: the client dies without ANY protocol
    // goodbye; the server keeps the PTY and child, and a reattach works.
    let scratch = Scratch::new("kill9");
    let mut h = ClientHarness::spawn(&scratch);
    h.wait_screen(15, |s| !s.trim().is_empty());
    h.type_bytes(b"SURVIVED=kill9\r");
    std::thread::sleep(Duration::from_millis(300));
    let pid = h.child.process_id().expect("client pid") as i32;
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    let _ = h.child.wait();
    drop(h);

    let mut h2 = ClientHarness::spawn(&scratch);
    h2.wait_screen(15, |s| !s.trim().is_empty());
    h2.type_bytes(b"echo var=$SURVIVED\r");
    h2.wait_screen(15, |s| s.lines().any(|l| l.trim() == "var=kill9"));
}

#[test]
fn persistence_dead_server_respawns_fresh_instead_of_hanging() {
    // AC3-ERR: the server is SIGKILLed (stale socket left behind). The next
    // client must detect it, print the one-line notice, spawn a fresh server,
    // and land in a NEW shell - never hang on the dead socket.
    let scratch = Scratch::new("respawn");
    let mut h = ClientHarness::spawn(&scratch);
    h.wait_screen(15, |s| !s.trim().is_empty());
    h.type_bytes(b"OLD_WORLD=yes\r");
    std::thread::sleep(Duration::from_millis(300));
    drop(h); // client gone first, so nothing redraws during the kill
    kill_server(&scratch);
    // The socket file survives the SIGKILL - that is the stale-socket case.
    assert!(
        scratch.main_sock().exists(),
        "SIGKILL must leave the stale socket behind for this test to mean anything"
    );

    let mut h2 = ClientHarness::spawn(&scratch);
    h2.wait_screen(15, |s| !s.trim().is_empty());
    // Fresh shell: the old environment is gone.
    h2.type_bytes(b"echo old=[$OLD_WORLD]\r");
    h2.wait_screen(15, |s| s.lines().any(|l| l.trim() == "old=[]"));
    // The one-line notice reached the user (printed before the TUI).
    assert!(
        h2.raw_output().contains("previous session ended"),
        "stale-socket notice missing from client output"
    );
}

#[test]
fn persistence_two_cold_clients_converge_on_one_server() {
    // AC4-EDGE / exit criterion 5: two clients launch simultaneously from a
    // cold start. Exactly one server may exist, and both clients must be
    // attached to it - proven structurally (process count) and semantically
    // (input typed in one client renders in the other).
    let scratch = Scratch::new("race");
    let mut a = ClientHarness::spawn(&scratch);
    let mut b = ClientHarness::spawn(&scratch);
    a.wait_screen(15, |s| !s.trim().is_empty());
    b.wait_screen(15, |s| !s.trim().is_empty());

    a.type_bytes(b"echo shared-pane-proof\r");
    a.wait_screen(15, |s| s.lines().any(|l| l.trim() == "shared-pane-proof"));
    b.wait_screen(15, |s| s.lines().any(|l| l.trim() == "shared-pane-proof"));

    assert_eq!(
        server_count(&scratch),
        1,
        "exactly one server may own the session"
    );
}

#[test]
fn persistence_client_relays_a_version_skew_refusal() {
    // The client half of the handshake contract: a server that refuses the
    // attach gets its reason relayed as a plain one-liner and a non-zero
    // exit - no TUI, no hang. Driven by a fake server so the refusal text is
    // under test control.
    let scratch = Scratch::new("skewrelay");
    let listener = std::os::unix::net::UnixListener::bind(scratch.main_sock()).unwrap();
    let accept_thread = std::thread::spawn(move || {
        if let Ok((mut conn, _)) = listener.accept() {
            let _: Result<ClientMsg, _> = read_msg_sync(&mut conn);
            let _ = write_msg_sync(
                &mut conn,
                &ServerMsg::Bye {
                    reason: "protocol version mismatch: simulated-skew-refusal".into(),
                },
            );
            // Give the client a moment to read before the socket drops.
            std::thread::sleep(Duration::from_millis(500));
        }
    });

    let mut h = ClientHarness::spawn(&scratch);
    let status = h.wait_exit(15);
    assert!(
        !status.success(),
        "a refused attach must exit non-zero, got {status:?}"
    );
    assert!(
        h.raw_output().contains("simulated-skew-refusal"),
        "refusal reason must reach the user; output: {}",
        h.raw_output()
    );
    accept_thread.join().unwrap();
}
