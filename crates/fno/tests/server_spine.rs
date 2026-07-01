//! Server-spine integration tests (task 1.2). The socket is the test seam:
//! each test spawns the real `fno --server <sock>` binary headless (no TTY
//! anywhere) and attaches raw `UnixStream` fake clients speaking the wire
//! protocol via the sync codec.

use std::io::ErrorKind;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use fno::proto::{
    read_msg_sync, write_msg_sync, ClientMsg, ServerMsg, BUILD_VERSION, PROTO_VERSION,
};
use fno::vt::frame_text;

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("fno-spine-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
    fn sock(&self) -> PathBuf {
        self.0.join("s.sock")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A server child that is always killed on test exit.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_server(sock: &Path, shell: &str) -> Server {
    let child = Command::new(env!("CARGO_BIN_EXE_fno"))
        .args(["--server"])
        .arg(sock)
        .env("SHELL", shell)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    Server(child)
}

/// Connect + Attach, returning the ready-to-use stream. Retries the connect
/// while the server boots.
fn attach(sock: &Path, rows: u16, cols: u16) -> UnixStream {
    let stream = connect_with_retry(sock);
    let mut s = stream.try_clone().unwrap();
    write_msg_sync(
        &mut s,
        &ClientMsg::Attach {
            proto: PROTO_VERSION,
            build: BUILD_VERSION.to_string(),
            rows,
            cols,
        },
    )
    .unwrap();
    stream
}

fn connect_with_retry(sock: &Path) -> UnixStream {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match UnixStream::connect(sock) {
            Ok(s) => return s,
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("server never came up at {}: {e}", sock.display()),
        }
    }
}

/// Read frames until `pred` matches one (returns its text), or panic with the
/// last screen at the deadline.
fn wait_for_frame(stream: &mut UnixStream, secs: u64, pred: impl Fn(&str) -> bool) -> String {
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut last = String::new();
    while Instant::now() < deadline {
        match read_msg_sync::<_, ServerMsg>(stream) {
            Ok(ServerMsg::Frame(f)) => {
                last = frame_text(&f);
                if pred(&last) {
                    return last;
                }
            }
            Ok(ServerMsg::Cursor { .. }) => {}
            Ok(ServerMsg::Bye { reason }) => panic!("unexpected Bye: {reason}"),
            Err(fno::proto::ProtoError::Io(e))
                if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {}
            Err(e) => panic!("read failed (last screen {last:?}): {e}"),
        }
    }
    panic!("no frame matched within {secs}s; last screen: {last:?}");
}

fn send(stream: &mut UnixStream, msg: &ClientMsg) {
    write_msg_sync(stream, msg).unwrap();
}

#[test]
fn server_spine_echo_roundtrips_via_fake_client() {
    // AC2-HP: keystrokes reach the PTY and the output renders, no TTY needed.
    let scratch = Scratch::new("echo");
    let _server = spawn_server(&scratch.sock(), "/bin/sh");
    let mut stream = attach(&scratch.sock(), 24, 80);
    // First frame = full resync of the current screen.
    wait_for_frame(&mut stream, 10, |_| true);
    send(&mut stream, &ClientMsg::Input(b"echo he\"ll\"o\r".to_vec()));
    // The typed line contains the quotes; only the OUTPUT is bare "hello".
    wait_for_frame(&mut stream, 10, |text| {
        text.lines().any(|l| l.trim() == "hello")
    });
}

#[test]
fn server_spine_unspawnable_shell_falls_back_to_sh() {
    // AC1-ERR: $SHELL points nowhere; the pane still comes up on /bin/sh.
    let scratch = Scratch::new("fallback");
    let _server = spawn_server(&scratch.sock(), "/nonexistent/not-a-shell");
    let mut stream = attach(&scratch.sock(), 24, 80);
    wait_for_frame(&mut stream, 10, |_| true);
    send(&mut stream, &ClientMsg::Input(b"echo fell-back\r".to_vec()));
    wait_for_frame(&mut stream, 10, |text| {
        text.lines().any(|l| l.trim() == "fell-back")
    });
}

#[test]
fn server_spine_exited_child_fails_closed_and_shows_state() {
    // AC2-ERR: after the child exits the pane shows an exited state and
    // further input is dropped - no panic, no crash, connection stays up.
    let scratch = Scratch::new("exited");
    let _server = spawn_server(&scratch.sock(), "/bin/sh");
    let mut stream = attach(&scratch.sock(), 24, 80);
    wait_for_frame(&mut stream, 10, |_| true);
    send(&mut stream, &ClientMsg::Input(b"exit\r".to_vec()));
    wait_for_frame(&mut stream, 10, |text| text.contains("[fno: pane exited]"));
    // Input after exit: dropped fail-closed. The server must still be alive
    // and serving this connection afterwards - prove it with a resize, which
    // always produces a fresh frame.
    send(&mut stream, &ClientMsg::Input(b"ignored\r".to_vec()));
    send(
        &mut stream,
        &ClientMsg::Resize {
            rows: 30,
            cols: 100,
        },
    );
    wait_for_frame(&mut stream, 10, |text| text.contains("[fno: pane exited]"));
}

#[test]
fn server_spine_output_flood_stays_responsive() {
    // AC2-EDGE: a 100k-line burst must not wedge input. The fake client
    // deliberately reads slowly relative to the flood (frames are droppable
    // and self-contained, so it only ever needs the newest), and input sent
    // MID-FLOOD must still land.
    let scratch = Scratch::new("flood");
    let _server = spawn_server(&scratch.sock(), "/bin/sh");
    let mut stream = attach(&scratch.sock(), 24, 80);
    wait_for_frame(&mut stream, 10, |_| true);
    send(
        &mut stream,
        &ClientMsg::Input(b"yes | head -100000; echo FLOOD-DONE\r".to_vec()),
    );
    // Mid-flood keystrokes (queued by the shell's line editor during the
    // flood, echoed at the next prompt).
    send(
        &mut stream,
        &ClientMsg::Input(b"echo after-flood\r".to_vec()),
    );
    wait_for_frame(&mut stream, 30, |text| {
        text.lines().any(|l| l.trim() == "after-flood")
    });
}

#[test]
fn server_spine_bad_client_dropped_peers_keep_streaming() {
    // AC4-ERR: client A dies abruptly (no Detach); the server keeps the PTY
    // and keeps serving client B.
    let scratch = Scratch::new("badclient");
    let _server = spawn_server(&scratch.sock(), "/bin/sh");
    let mut a = attach(&scratch.sock(), 24, 80);
    wait_for_frame(&mut a, 10, |_| true);
    let mut b = attach(&scratch.sock(), 24, 80);
    wait_for_frame(&mut b, 10, |_| true);
    // A vanishes without ceremony.
    drop(a);
    // The pane keeps working for B.
    send(&mut b, &ClientMsg::Input(b"echo b-survives\r".to_vec()));
    wait_for_frame(&mut b, 10, |text| {
        text.lines().any(|l| l.trim() == "b-survives")
    });
}

#[test]
fn server_spine_version_skew_is_refused_with_both_versions() {
    // The version handshake at the wire level: a mismatched Attach gets a
    // loud Bye naming both sides, and the connection closes.
    let scratch = Scratch::new("skew");
    let _server = spawn_server(&scratch.sock(), "/bin/sh");
    let mut stream = connect_with_retry(&scratch.sock());
    write_msg_sync(
        &mut stream,
        &ClientMsg::Attach {
            proto: PROTO_VERSION + 7,
            build: "99.0.0".to_string(),
            rows: 24,
            cols: 80,
        },
    )
    .unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    match read_msg_sync::<_, ServerMsg>(&mut stream) {
        Ok(ServerMsg::Bye { reason }) => {
            assert!(reason.contains("99.0.0"), "{reason}");
            assert!(reason.contains(&format!("v{PROTO_VERSION}")), "{reason}");
        }
        other => panic!("expected Bye, got {other:?}"),
    }
}
