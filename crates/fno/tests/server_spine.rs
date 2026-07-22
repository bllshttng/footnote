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
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_fno"));
    cmd.args(["--server"])
        .arg(sock)
        .env("SHELL", shell)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Isolate the agent registry, claude-daemon roster, and squad store to empty
    // scratch subdirs (same idiom as common::spawn_server): the spawned binary is
    // built without cfg(test), so it would otherwise read the developer's real
    // ~/.fno/squads.json and inherit live named squads, keeping the session alive
    // after the last child exits (no Bye) - a machine with saved squads fails; a
    // clean CI home hides it.
    let iso = sock.parent().unwrap_or_else(|| Path::new("."));
    cmd.env("FNO_AGENTS_HOME", iso.join("iso-agents"));
    cmd.env("FNO_CLAUDE_DAEMON_DIR", iso.join("iso-daemon"));
    cmd.env(
        "FNO_GLOBAL_SETTINGS_PATH",
        iso.join("iso-cfg").join("settings.json"),
    );
    Server(cmd.spawn().unwrap())
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
            cwd: "/".to_string(),
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
    let f = wait_for_raw_frame(stream, secs, |f| pred(&frame_text(f)));
    frame_text(&f)
}

/// Same, but the predicate sees the whole `Frame` (geometry included).
fn wait_for_raw_frame(
    stream: &mut UnixStream,
    secs: u64,
    pred: impl Fn(&fno::proto::Frame) -> bool,
) -> fno::proto::Frame {
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut last = String::new();
    while Instant::now() < deadline {
        match read_msg_sync::<_, ServerMsg>(stream) {
            Ok(ServerMsg::Frame { frame, .. }) => {
                last = frame_text(&frame);
                if pred(&frame) {
                    return frame;
                }
            }
            // Reliable-channel messages (Layout/ModeSync/Notice) are asserted
            // by the layout e2e suite (task 2.6), not the spine helpers.
            Ok(ServerMsg::Layout { .. })
            | Ok(ServerMsg::ModeSync { .. })
            | Ok(ServerMsg::Notice { .. })
            | Ok(ServerMsg::Info { .. })
            | Ok(ServerMsg::PaneList { .. })
            | Ok(ServerMsg::PaneText { .. })
            | Ok(ServerMsg::PaneSpawned { .. })
            | Ok(ServerMsg::Ok)
            | Ok(ServerMsg::WaitDone { .. })
            | Ok(ServerMsg::Err { .. })
            | Ok(ServerMsg::Copy { .. })
            | Ok(ServerMsg::SearchResult { .. })
            | Ok(ServerMsg::PeekBody { .. })
            | Ok(ServerMsg::TabList { .. })
            | Ok(ServerMsg::LayoutTree { .. })
            | Ok(ServerMsg::PaneLocation { .. })
            | Ok(ServerMsg::TabSpawned { .. })
            | Ok(ServerMsg::LayoutApplied { .. }) => {}
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
fn server_spine_exited_child_ends_the_session_with_bye() {
    // Locked Decision 8 (supersedes Phase 1's rendered "exited" state): the
    // child exiting closes its pane; the last pane of the last tab of the
    // last squad ends the session - the client receives Bye and the server
    // process exits cleanly.
    let scratch = Scratch::new("exited");
    let mut server = spawn_server(&scratch.sock(), "/bin/sh");
    let mut stream = attach(&scratch.sock(), 24, 80);
    wait_for_frame(&mut stream, 10, |_| true);
    send(&mut stream, &ClientMsg::Input(b"exit\r".to_vec()));

    // Read past frames/layouts until the Bye lands.
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match read_msg_sync::<_, ServerMsg>(&mut stream) {
            Ok(ServerMsg::Bye { reason }) => {
                assert!(reason.contains("session ended"), "{reason}");
                break;
            }
            Ok(_) => {}
            Err(fno::proto::ProtoError::Io(e))
                if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {}
            Err(e) => panic!("expected Bye, got error: {e}"),
        }
        if Instant::now() >= deadline {
            panic!("no Bye within 10s of the last pane's child exiting");
        }
    }

    // The server process itself exits 0 (session over, socket unlinked).
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = server.0.try_wait().unwrap() {
            assert!(status.success(), "server must exit 0, got {status:?}");
            break;
        }
        if Instant::now() >= deadline {
            panic!("server did not exit after the session ended");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
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
    // Only send the mid-flood keystrokes once a frame PROVES the flood is
    // rendering (a screen of y-lines); otherwise both inputs could queue
    // before the flood starts and "input works during the flood" would be
    // asserted without being exercised. The FLOOD-DONE alternative keeps the
    // test deterministic on a machine fast enough that our reader only ever
    // sees the final frame (frames are droppable; we cannot force one).
    wait_for_frame(&mut stream, 15, |text| {
        text.lines().filter(|l| l.trim() == "y").count() >= 5 || text.contains("FLOOD-DONE")
    });
    // The typed line carries quotes so only the OUTPUT is bare, and the
    // match is contains-based: input landing during the pipeline teardown
    // can lose its echo-back to the tty line-discipline flush (the Phase 1
    // codex P1 class), gluing the output onto the prompt line
    // ("$ after-flood") - the input still landed, which is what this test
    // proves.
    send(
        &mut stream,
        &ClientMsg::Input(b"echo af\"ter\"-flood\r".to_vec()),
    );
    wait_for_frame(&mut stream, 30, |text| {
        text.lines()
            .any(|l| l.contains("after-flood") && !l.contains('"'))
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
            cwd: "/".to_string(),
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
