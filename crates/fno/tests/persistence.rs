//! Persistence + race tests (task 1.4): scripted versions of the Phase-1
//! manual exit criteria. Detach/reattach exactness, kill -9 survival, dead-
//! server respawn, the two-client cold-start race, and the client-side
//! version-skew refusal - all through the client-under-portable-pty harness.

mod common;

use std::sync::Mutex;
use std::time::Duration;

use common::{connect_with_retry, spawn_server, ClientHarness, FakeClient, Scratch};
use fno::proto::{
    read_msg_sync, write_msg_sync, Cell, ClientMsg, ControlVerb, Frame, ServerMsg, BUILD_VERSION,
    PROTO_VERSION,
};

/// Serializes this file's PTY-spawning tests. `cargo test --all-targets` runs
/// them in parallel by default, and the real PTYs + Unix sockets contend for
/// the runner's CPU, which reddens rust-ci on unrelated PRs. A module-local
/// gate (no `serial_test` dep) makes exactly these tests run one at a time.
/// The gate guards `()`, not data, and each test owns a unique scratch cleaned
/// up on `Drop`, so a panicking test corrupts nothing shared: take the lock
/// with `into_inner()` to ignore poisoning, keeping the real first failure
/// visible instead of burying it under a cascade of `PoisonError` panics.
static PTY_GATE: Mutex<()> = Mutex::new(());

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

fn pane_send(scratch: &Scratch, pane: u64, bytes: &[u8]) {
    let mut stream = connect_with_retry(&scratch.main_sock());
    write_msg_sync(
        &mut stream,
        &ClientMsg::Control {
            proto: PROTO_VERSION,
            build: BUILD_VERSION.to_string(),
            verb: ControlVerb::PaneSend {
                pane,
                bytes: bytes.to_vec(),
                guarded: false,
            },
        },
    )
    .unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    match read_msg_sync::<_, ServerMsg>(&mut stream) {
        Ok(ServerMsg::Ok) => {}
        other => panic!("pane send failed: {other:?}"),
    }
}

#[test]
fn persistence_reattach_restores_the_exact_screen() {
    let _g = PTY_GATE.lock().unwrap_or_else(|e| e.into_inner());
    // AC3-HP + AC3-UI: reattach lands on the SAME live PTY and the full
    // screen redraws from the server grid - the new client's settled screen
    // is byte-identical to what the old client saw at detach.
    let scratch = Scratch::new("exact");
    let mut h = ClientHarness::spawn(&scratch);
    h.wait_prompt(15);
    h.type_bytes(b"echo marker-one; echo marker-two\r");
    h.wait_screen(15, |s| s.lines().any(|l| l.trim() == "marker-two"));
    // Snapshot only once the screen has been STABLE for two consecutive
    // polls, so a late-rendering prompt can never make the byte-exact
    // comparison below unreachable on a loaded runner.
    let before = {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let a = h.screen();
            std::thread::sleep(Duration::from_millis(150));
            let b = h.screen();
            if a == b {
                break b;
            }
            if std::time::Instant::now() >= deadline {
                panic!("screen never settled; last:\n{b}");
            }
        }
    };
    h.type_bytes(b"\x02d"); // leader+d detach
    assert!(h.wait_exit(10).success());
    drop(h);

    let mut h2 = ClientHarness::spawn(&scratch);
    h2.wait_screen(15, |s| s == before);
}

#[test]
fn persistence_alt_screen_program_survives_detach_reattach() {
    let _g = PTY_GATE.lock().unwrap_or_else(|e| e.into_inner());
    // AC3-EDGE: a full-screen alt-screen program (vim's mechanism: DECSET
    // 1049) is running at detach; on reattach its screen renders correctly
    // because the SERVER holds the alt-screen grid. Driven with printf+cat so
    // the test needs no vim binary, only the exact escape sequence vim emits.
    let scratch = Scratch::new("altscreen");
    let mut h = ClientHarness::spawn(&scratch);
    h.wait_prompt(15);
    // Enter alt screen, clear, home, draw a marker; cat holds the program
    // "open" in the foreground exactly like an editor session.
    h.type_bytes(b"printf '\\033[?1049h\\033[2J\\033[HALT-SCREEN-HELD'; cat\r");
    h.wait_screen(15, |s| s.contains("ALT-SCREEN-HELD"));
    h.type_bytes(b"\x02d"); // leader+d: detach with the alt screen active
    assert!(h.wait_exit(10).success());
    drop(h);

    let mut h2 = ClientHarness::spawn(&scratch);
    let screen = h2.wait_screen(15, |s| s.contains("ALT-SCREEN-HELD"));
    // The alt screen starts with the marker at the top of the CONTENT area -
    // the first row under the tab bar - with no garbled partial.
    let content_top = screen.lines().nth(1).unwrap_or_default();
    assert!(
        content_top.trim_start().starts_with("ALT-SCREEN-HELD"),
        "alt screen must redraw from the content top: {screen:?}"
    );
    // Leave the program: ^C ends cat; the shell must still be there. Wait for
    // the prompt before typing - bytes sent while cat still holds the
    // foreground can be flushed by the line discipline instead of executed.
    h2.type_bytes(&[0x03]);
    h2.wait_prompt(15);
    h2.type_bytes(b"printf '\\033[?1049l'; echo back-on-main\r");
    h2.wait_screen(15, |s| s.lines().any(|l| l.trim() == "back-on-main"));
}

#[test]
fn persistence_multi_pane_reattach_is_screen_exact() {
    let _g = PTY_GATE.lock().unwrap_or_else(|e| e.into_inner());
    // AC3-HP/AC5-FR generalized to N panes through the REAL client: build a
    // split via leader chords, put distinct markers in both panes, detach,
    // reattach - the settled screen (chrome + both panes) is byte-identical.
    let scratch = Scratch::new("multipane");
    let mut h = ClientHarness::spawn(&scratch);
    h.wait_prompt(15);
    h.type_bytes(b"\x02%"); // leader+% : split H, focus lands right
    h.wait_screen(15, |s| s.lines().skip(1).any(|l| l.contains('│')));
    h.type_bytes(b"echo marker-right\r");
    h.wait_screen(15, |s| s.contains("marker-right"));
    h.type_bytes(b"\x02h"); // leader+h : focus left
    h.type_bytes(b"echo marker-left\r");
    h.wait_screen(15, |s| s.contains("marker-left"));
    let before = {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let a = h.screen();
            std::thread::sleep(Duration::from_millis(150));
            let b = h.screen();
            if a == b {
                break b;
            }
            if std::time::Instant::now() >= deadline {
                panic!("screen never settled; last:\n{b}");
            }
        }
    };
    h.type_bytes(b"\x02d"); // leader+d detach
    assert!(h.wait_exit(10).success());
    drop(h);

    let mut h2 = ClientHarness::spawn(&scratch);
    h2.wait_screen(15, |s| s == before);
}

#[test]
fn persistence_kill_nine_of_the_client_leaves_the_pty_running() {
    let _g = PTY_GATE.lock().unwrap_or_else(|e| e.into_inner());
    // AC4-HP / exit criterion 3: the client dies without ANY protocol
    // goodbye; the server keeps the PTY and child, and a reattach works.
    let scratch = Scratch::new("kill9");
    let mut h = ClientHarness::spawn(&scratch);
    h.wait_prompt(15);
    // The echo marker proves the assignment traversed client -> server ->
    // PTY -> shell BEFORE the kill; a bare sleep could race a loaded runner.
    h.type_bytes(b"SURVIVED=kill9; echo set-ok\r");
    h.wait_screen(15, |s| s.lines().any(|l| l.trim() == "set-ok"));
    let pid = h.child.process_id().expect("client pid") as i32;
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    let _ = h.child.wait();
    drop(h);

    let mut h2 = ClientHarness::spawn(&scratch);
    h2.wait_prompt(15);
    h2.type_bytes(b"echo var=$SURVIVED\r");
    h2.wait_screen(15, |s| s.lines().any(|l| l.trim() == "var=kill9"));
}

#[test]
fn persistence_dead_server_respawns_fresh_instead_of_hanging() {
    let _g = PTY_GATE.lock().unwrap_or_else(|e| e.into_inner());
    // AC3-ERR: the server is SIGKILLed (stale socket left behind). The next
    // client must detect it, print the one-line notice, spawn a fresh server,
    // and land in a NEW shell - never hang on the dead socket.
    let scratch = Scratch::new("respawn");
    let mut h = ClientHarness::spawn(&scratch);
    h.wait_prompt(15);
    h.type_bytes(b"OLD_WORLD=yes\r");
    std::thread::sleep(Duration::from_millis(300));
    let h_diag = h.diagnostics(); // captured pre-drop; h is gone at the assert
    drop(h); // client gone first, so nothing redraws during the kill
    kill_server(&scratch);
    // The socket file survives the SIGKILL - that is the stale-socket case.
    // A missing socket here means the server already exited CLEANLY (its
    // SocketGuard unlinked) before the pkill, or never came up at all - the
    // captured diagnostics name why (x-0296).
    assert!(
        scratch.main_sock().exists(),
        "SIGKILL must leave the stale socket behind for this test to mean anything\n{h_diag}"
    );

    let mut h2 = ClientHarness::spawn(&scratch);
    h2.wait_prompt(15);
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
    let _g = PTY_GATE.lock().unwrap_or_else(|e| e.into_inner());
    // AC4-EDGE / exit criterion 5: two clients launch simultaneously from a
    // cold start. Exactly one server may exist, and both clients must be
    // attached to it - proven structurally (process count) and semantically
    // (input typed in one client renders in the other).
    let scratch = Scratch::new("race");
    let mut a = ClientHarness::spawn(&scratch);
    let mut b = ClientHarness::spawn(&scratch);
    a.wait_prompt(15);
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
fn persistence_malformed_frame_is_rejected_not_panicked() {
    let _g = PTY_GATE.lock().unwrap_or_else(|e| e.into_inner());
    // The wire trust boundary: a Frame whose cell count disagrees with its
    // geometry must be refused like a malformed message - a clear one-liner
    // and a non-zero exit, never a slice panic inside the alternate screen.
    let scratch = Scratch::new("badframe");
    let listener = std::os::unix::net::UnixListener::bind(scratch.main_sock()).unwrap();
    let accept_thread = std::thread::spawn(move || {
        if let Ok((mut conn, _)) = listener.accept() {
            let _: Result<ClientMsg, _> = read_msg_sync(&mut conn);
            let bad = Frame {
                rows: 24,
                cols: 80,
                cells: vec![Cell::default(); 10], // 10 != 24*80
                cursor_row: 0,
                cursor_col: 0,
                cursor_visible: true,
                scroll_offset: 0,
            };
            let _ = write_msg_sync(
                &mut conn,
                &ServerMsg::Frame {
                    pane_id: 0,
                    frame: bad,
                },
            );
            std::thread::sleep(Duration::from_millis(500));
        }
    });

    // Attach "main" directly: the fake server accepts exactly once, so the
    // pre-attach picker's live/stale probe (bare `fno`) would consume it
    // before the real attach. Naming the session bypasses the picker (AC5-FR).
    let mut h = ClientHarness::spawn_session(&scratch, "main");
    let status = h.wait_exit(15);
    assert!(
        !status.success(),
        "a malformed frame must exit non-zero, got {status:?}"
    );
    assert!(
        h.raw_output().contains("malformed frame"),
        "rejection must name the cause; output: {}",
        h.raw_output()
    );
    accept_thread.join().unwrap();
}

#[test]
fn persistence_client_relays_a_version_skew_refusal() {
    let _g = PTY_GATE.lock().unwrap_or_else(|e| e.into_inner());
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

    // Direct attach to "main" (see the malformed-frame test): the fake
    // server accepts once, so bypass the pre-attach picker's probe (AC5-FR).
    let mut h = ClientHarness::spawn_session(&scratch, "main");
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

// -- Phase 3 (task 3.6): lifecycle - persistence is the product --------------

#[test]
fn persistence_zero_client_session_survives_and_resyncs_fully() {
    let _g = PTY_GATE.lock().unwrap_or_else(|e| e.into_inner());
    // AC6-HP + AC6-UI: every client detaches; the server holds identical
    // pane state ("hours" compressed to a beat), `mux ls` shows the
    // persistent session with clients=0 and live pane counts, and a
    // reattach resyncs Layout + full frames.
    let scratch = Scratch::new("zeroclient");
    let _server = spawn_server(&scratch.main_sock(), &[("SHELL", "/bin/sh")]);
    let cwd = scratch.0.join("w");
    std::fs::create_dir_all(&cwd).unwrap();

    let mut c = FakeClient::attach(&scratch.main_sock(), 24, 80, cwd.to_str().unwrap());
    let pane = c
        .wait_layout(10, "first layout", |l| l.panes.len() == 1)
        .focus;
    c.wait_prompt(pane);
    c.input(b"echo survives-detach#\r");
    c.wait_pane_text(15, pane, |t| {
        t.lines().any(|l| l.trim() == "survives-detach#")
    });
    c.detach();
    drop(c);
    std::thread::sleep(Duration::from_millis(800));

    // Persistence is visible, not spooky (AC6-UI).
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_fno"))
        .args(["mux", "ls"])
        .env("FNO_MUX_DIR", &scratch.0)
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("main: 0 clients, 1 squads, 1 panes"),
        "zero-client persistence must be listable; got: {stdout}"
    );

    // Full resync on reattach: Layout, then the frame carries the old
    // marker without any new input.
    let mut c2 = FakeClient::attach(&scratch.main_sock(), 24, 80, cwd.to_str().unwrap());
    c2.wait_layout(10, "reattach layout", |l| l.panes.len() == 1);
    c2.wait_pane_text(15, pane, |t| t.contains("survives-detach#"));
}

#[test]
fn persistence_last_pane_exit_with_zero_clients_ends_the_server() {
    let _g = PTY_GATE.lock().unwrap_or_else(|e| e.into_inner());
    // AC6-ERR: the last pane's child exits while NO client is attached -
    // the server exits 0 (nobody to Bye) and unlinks its socket (Locked 12's
    // first exit path, with the client count at zero).
    let scratch = Scratch::new("lastpane");
    let mut server = spawn_server(&scratch.main_sock(), &[("SHELL", "/bin/sh")]);
    let cwd = scratch.0.join("w");
    std::fs::create_dir_all(&cwd).unwrap();

    let mut c = FakeClient::attach(&scratch.main_sock(), 24, 80, cwd.to_str().unwrap());
    let pane = c
        .wait_layout(10, "first layout", |l| l.panes.len() == 1)
        .focus;
    c.wait_prompt(pane);
    // Arm the exit behind a shell read, prove the command reached the shell,
    // then detach. A control send releases the read after the client is gone,
    // so the pane child exits while the registered client count is zero.
    c.input(b"echo armed#; read _; exit\r");
    c.wait_pane_text(15, pane, |t| t.lines().any(|l| l.trim() == "armed#"));
    c.detach();
    drop(c);
    pane_send(&scratch, pane, b"\r");

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(s) = server.0.try_wait().unwrap() {
            break s;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "server must exit after its last pane dies"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "clean exit 0, got {status:?}");
    assert!(
        !scratch.main_sock().exists(),
        "the SocketGuard must unlink the socket on the way out"
    );
}

/// The external-lifecycle store's PRODUCTION path (x-7561): the `#[cfg(test)]`
/// thread-local store override is compiled OUT of the library when it is a
/// dependency, so this integration test is the only place `squads_path()`'s
/// real `FNO_AGENTS_HOME` branch is exercised. A begin-stop -> complete
/// round-trips through the real `squads.json` (each `load()` re-reads the file),
/// proving durability across the CAS boundary. Env is set + restored under
/// `PTY_GATE` so it never races the file's other serialized tests.
#[test]
fn external_lifecycle_round_trips_through_the_production_store() {
    use fno::squad_store::{
        begin_external_stop, complete_external, load, ExternalState, LifecycleCas,
    };

    let _gate = PTY_GATE.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("fno-ext-lc-int-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let prev = std::env::var_os("FNO_AGENTS_HOME");
    std::env::set_var("FNO_AGENTS_HOME", &dir);

    let loaded = |id: &str| {
        load()
            .external_lifecycle
            .into_iter()
            .find(|r| r.attach_id == id)
            .map(|r| r.state)
    };

    assert!(matches!(
        begin_external_stop("deadbeef", "ext", "/tmp").unwrap(),
        LifecycleCas::Committed { generation: 1 }
    ));
    assert_eq!(loaded("deadbeef"), Some(ExternalState::Stopping));
    complete_external("deadbeef", 1, ExternalState::Stopping, true, None).unwrap();
    assert_eq!(loaded("deadbeef"), Some(ExternalState::Stopped));
    assert!(
        dir.join("squads.json").exists(),
        "the record lands at the production squads.json path"
    );

    match prev {
        Some(v) => std::env::set_var("FNO_AGENTS_HOME", v),
        None => std::env::remove_var("FNO_AGENTS_HOME"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}
