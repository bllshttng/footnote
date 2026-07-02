//! Client e2e tests (task 1.3): the real `fno` client runs under a
//! portable-pty from this harness - a true TTY, so bare `fno` role-selects to
//! the client, spawns its server, attaches, and draws. The harness plays the
//! human: it types into the PTY master and reads the client's rendered output
//! through our own VT emulator (`fno::vt::Pane`), i.e. it asserts on exactly
//! the screen a person would see.

mod common;

use std::time::Duration;

use common::{ClientHarness, Scratch};

#[test]
fn client_e2e_prompt_appears_and_echo_roundtrips() {
    // AC1-HP + AC2-HP: bare `fno` on a TTY comes up with a shell, and typed
    // input round-trips to rendered output. (The 500ms latency target is not
    // asserted - CI wall-clock is not a fairness court; presence is.)
    let scratch = Scratch::new("echo");
    let mut h = ClientHarness::spawn(&scratch);
    // A prompt renders.
    h.wait_screen(15, |s| !s.trim().is_empty());
    h.type_bytes(b"echo he\"ll\"o\r");
    // Only the OUTPUT line is bare "hello" (the typed line has quotes).
    h.wait_screen(15, |s| s.lines().any(|l| l.trim() == "hello"));
    // AC1-UI: the cursor is visible and sits on the last written row (the
    // fresh prompt), where the shell put it.
    let frame = h.pane.frame();
    assert!(frame.cursor_visible, "cursor must be visible at the prompt");
    let text = h.screen();
    let last_row = text.lines().count().saturating_sub(1);
    assert_eq!(
        frame.cursor_row as usize, last_row,
        "cursor should sit on the prompt row; screen:\n{text}"
    );
}

#[test]
fn client_e2e_utf8_and_control_keys_pass_through() {
    // AC2-UI: UTF-8 renders; Ctrl-C interrupts a foreground command. Both are
    // raw-byte passthrough - nothing re-encodes the input.
    let scratch = Scratch::new("bytes");
    let mut h = ClientHarness::spawn(&scratch);
    h.wait_screen(15, |s| !s.trim().is_empty());
    h.type_bytes("echo caf\u{00e9}\r".as_bytes());
    h.wait_screen(15, |s| s.lines().any(|l| l.trim() == "caf\u{00e9}"));
    // Ctrl-C a sleep; the shell survives and answers again.
    h.type_bytes(b"sleep 100\r");
    std::thread::sleep(Duration::from_millis(300));
    h.type_bytes(&[0x03]); // ^C
    h.type_bytes(b"echo interrupted\r");
    h.wait_screen(15, |s| s.lines().any(|l| l.trim() == "interrupted"));
}

#[test]
fn client_e2e_detach_exits_client_and_leaves_server_running() {
    // The Ctrl-\ detach: client exits 0, and the session (server + shell)
    // stays alive - proven by a fresh client reattaching and seeing state
    // from before the detach. (Full persistence torture is task 1.4.)
    let scratch = Scratch::new("detach");
    let mut h = ClientHarness::spawn(&scratch);
    h.wait_screen(15, |s| !s.trim().is_empty());
    h.type_bytes(b"BEFORE_DETACH=yes\r");
    std::thread::sleep(Duration::from_millis(300));
    h.type_bytes(&[0x1C]); // Ctrl-\ -> detach
    let status = h.wait_exit(10);
    assert!(status.success(), "detach must exit 0, got {status:?}");
    drop(h);

    // Reattach with a new client on the same session.
    let mut h2 = ClientHarness::spawn(&scratch);
    h2.wait_screen(15, |s| !s.trim().is_empty());
    h2.type_bytes(b"echo var=$BEFORE_DETACH\r");
    h2.wait_screen(15, |s| s.lines().any(|l| l.trim() == "var=yes"));
}
