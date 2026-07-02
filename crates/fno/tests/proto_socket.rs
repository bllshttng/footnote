//! Socket-lifecycle integration tests for the mux protocol (task 1.1).
//!
//! These exercise the real filesystem + real Unix sockets in a per-test
//! tempdir (socket paths stay short - the sun_path limit is ~104 bytes on
//! macOS), plus the built `fno` binary for the non-TTY gate.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use fno::proto::{bind_or_probe, BindOutcome};

/// A unique short-lived scratch dir. No tempfile dep: pid + test name is
/// unique enough for a test process, and the dir is removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("fno-proto-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    fn path(&self, file: &str) -> PathBuf {
        self.0.join(file)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn proto_fresh_bind_wins() {
    let scratch = Scratch::new("fresh");
    let sock = scratch.path("s.sock");
    match bind_or_probe(&sock).unwrap() {
        BindOutcome::Bound(_) => {}
        BindOutcome::AlreadyRunning => panic!("fresh path must bind"),
    }
}

#[test]
fn proto_live_server_is_detected_not_clobbered() {
    let scratch = Scratch::new("live");
    let sock = scratch.path("s.sock");
    let listener = match bind_or_probe(&sock).unwrap() {
        BindOutcome::Bound(l) => l,
        BindOutcome::AlreadyRunning => panic!("fresh path must bind"),
    };
    // A second bind attempt while the listener lives must yield AlreadyRunning
    // and must NOT unlink the live socket.
    match bind_or_probe(&sock).unwrap() {
        BindOutcome::AlreadyRunning => {}
        BindOutcome::Bound(_) => panic!("live socket was clobbered"),
    }
    assert!(sock.exists(), "live socket file must survive the probe");
    // The original listener still accepts.
    listener.set_nonblocking(true).unwrap();
    let _client = UnixStream::connect(&sock).unwrap();
    drop(listener);
}

#[test]
fn proto_stale_socket_is_unlinked_and_rebound() {
    let scratch = Scratch::new("stale");
    let sock = scratch.path("s.sock");
    // Bind then drop: the listener dies but the socket FILE stays - exactly
    // what a killed server leaves behind.
    match bind_or_probe(&sock).unwrap() {
        BindOutcome::Bound(l) => drop(l),
        BindOutcome::AlreadyRunning => panic!("fresh path must bind"),
    }
    assert!(sock.exists(), "dropping a listener must leave the file");
    match bind_or_probe(&sock).unwrap() {
        BindOutcome::Bound(_) => {}
        BindOutcome::AlreadyRunning => panic!("stale socket must be rebindable"),
    }
}

#[test]
fn proto_bind_race_converges_on_exactly_one_server() {
    let scratch = Scratch::new("race");
    let sock = scratch.path("s.sock");
    // AC4-EDGE: N cold-start racers, exactly ONE Bound. A barrier maximizes
    // the simultaneity of the bind attempts.
    let n = 8;
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(n));
    let mut handles = Vec::new();
    for _ in 0..n {
        let sock = sock.clone();
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            match bind_or_probe(&sock) {
                Ok(BindOutcome::Bound(l)) => {
                    // Hold the listener so probes see a live server.
                    std::thread::sleep(std::time::Duration::from_millis(400));
                    drop(l);
                    1
                }
                Ok(BindOutcome::AlreadyRunning) => 0,
                Err(e) => panic!("bind_or_probe errored in race: {e}"),
            }
        }));
    }
    let winners: i32 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    assert_eq!(winners, 1, "exactly one racer may own the socket");
}

#[test]
fn proto_non_tty_bare_fno_prints_notice_and_exits_zero() {
    // AC1-EDGE / exit criterion 6: `fno < /dev/null` never opens a TUI.
    let out = Command::new(env!("CARGO_BIN_EXE_fno"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(out.status.success(), "must exit 0, got {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("not a tty"), "notice missing: {stdout:?}");
}
