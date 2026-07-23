//! Socket-lifecycle integration tests for the mux protocol (task 1.1).
//!
//! These exercise the real filesystem + real Unix sockets in a per-test
//! tempdir (socket paths stay short - the sun_path limit is ~104 bytes on
//! macOS), plus the built `fno` binary for the non-TTY gate.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use std::time::Duration;

use fno::proto::{bind_or_probe, connect_unix_timeout, BindOutcome};

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
fn proto_v28_commands_round_trip_over_the_wire() {
    // x-7561: the new v28 commands (ReapAgents + the external lifecycle verbs)
    // serialize and deserialize byte-for-byte through the length-prefixed
    // framing a real client/server pair uses.
    use fno::proto::{read_msg_sync, write_msg_sync, ClientMsg, Command};

    let cmds = [
        Command::ReapAgents,
        Command::StopExternal {
            attach_id: "deadbeef".into(),
            name: "ext-a".into(),
        },
        Command::RemoveExternal {
            attach_id: "cafef00d".into(),
            name: "ext-b".into(),
        },
    ];
    for cmd in cmds {
        let mut buf: Vec<u8> = Vec::new();
        write_msg_sync(&mut buf, &ClientMsg::Command(cmd.clone())).unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let decoded: ClientMsg = read_msg_sync(&mut cur).unwrap();
        assert_eq!(decoded, ClientMsg::Command(cmd));
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

// ---------------------------------------------------------------------------
// Bounded client connects (x-2896): a wedged server (live listener, dead
// accept loop, FULL backlog) must produce a bounded error, never a hang.

/// How a saturated backlog manifests to a client connect. It differs by
/// platform, and BOTH are bounded (the anti-hang guarantee) - only the error
/// kind differs, which the call sites key on:
/// - `Timeout`: Linux AF_UNIX blocks a connect to a full backlog; the
///   nonblocking-connect + poll turns that into `TimedOut` (also the EAGAIN
///   path). A wedged server reads as alive here.
/// - `Refused`: macOS AF_UNIX refuses a connect to a full/zero backlog
///   (`ECONNREFUSED`), indistinguishable from a dead server - so a wedged
///   server IS clobberable there (pre-existing, and effectively fine: a
///   full-backlog dead-accept-loop server is already unusable).
#[derive(PartialEq, Debug)]
enum WedgeKind {
    Timeout,
    Refused,
}

/// A listener that never accepts, with its backlog saturated: the wedged-
/// server fixture. Holds the queued streams so they stay pending.
struct WedgedServer {
    fd: std::os::fd::RawFd,
    kind: WedgeKind,
    _queued: Vec<UnixStream>,
}

impl WedgedServer {
    /// Build the fixture or FAIL the test - a fixture that cannot establish its
    /// precondition (a saturated backlog) is a broken fixture, never a silent
    /// skip that would let the primary regression pass vacuously.
    fn new(path: &std::path::Path) -> Self {
        use std::os::unix::ffi::OsStrExt;
        let fd = unsafe {
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
            assert!(fd >= 0, "socket() failed");
            let mut addr: libc::sockaddr_un = std::mem::zeroed();
            addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
            let bytes = path.as_os_str().as_bytes();
            assert!(bytes.len() < std::mem::size_of_val(&addr.sun_path));
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr() as *const libc::c_char,
                addr.sun_path.as_mut_ptr(),
                bytes.len(),
            );
            let len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
            assert_eq!(
                libc::bind(fd, &addr as *const _ as *const libc::sockaddr, len),
                0,
                "bind() failed"
            );
            // Use an explicit one-slot queue and never call accept(). A zero
            // backlog has platform-specific/default semantics on macOS and
            // can admit connections until the process hits EMFILE instead of
            // creating the bounded wedge this fixture needs.
            assert_eq!(libc::listen(fd, 1), 0, "listen() failed");
            fd
        };
        // Saturate the queue: connects succeed (kernel-queued) until full,
        // then the next connect either times out (Linux blocks) or is refused
        // (macOS). Either is the wedge - record which so the tests can assert
        // the platform-honest outcome. A backlog of 1 saturates within a
        // couple of connects; 256 is far more headroom than any real kernel
        // needs, so exhausting it means the fixture's assumption broke and the
        // test MUST fail (not silently skip).
        let mut queued = Vec::new();
        for _ in 0..256 {
            match connect_unix_timeout(path, Duration::from_millis(250)) {
                Ok(s) => queued.push(s),
                Err(e) => {
                    let kind = match e.kind() {
                        std::io::ErrorKind::TimedOut => WedgeKind::Timeout,
                        std::io::ErrorKind::ConnectionRefused => WedgeKind::Refused,
                        other => {
                            // Close fd before the panic unwinds - it is not yet
                            // owned by a WedgedServer, so RAII would not reclaim it.
                            unsafe { libc::close(fd) };
                            panic!("unexpected error while saturating backlog: {other:?} ({e})");
                        }
                    };
                    return WedgedServer {
                        fd,
                        kind,
                        _queued: queued,
                    };
                }
            }
        }
        unsafe { libc::close(fd) };
        panic!("backlog did not saturate in 256 connects - fixture assumption broke");
    }
}

impl Drop for WedgedServer {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

#[test]
fn connect_timeout_wedged_listener_bounded_error_not_hang() {
    let scratch = Scratch::new("wedged");
    let sock = scratch.path("s.sock");
    let wedged = WedgedServer::new(&sock);
    // The anti-hang guarantee: a connect to a wedged server returns a BOUNDED
    // error fast, never blocks. The exact kind is the platform's saturated-
    // backlog manifestation (TimedOut on Linux, ConnectionRefused on macOS).
    let start = std::time::Instant::now();
    let err = connect_unix_timeout(&sock, Duration::from_millis(300))
        .expect_err("full backlog must not connect");
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "bounded connect took {:?}",
        start.elapsed()
    );
    let expected = match wedged.kind {
        WedgeKind::Timeout => std::io::ErrorKind::TimedOut,
        WedgeKind::Refused => std::io::ErrorKind::ConnectionRefused,
    };
    assert_eq!(err.kind(), expected, "wedge={:?} got: {err}", wedged.kind);
}

#[test]
fn connect_timeout_stale_socket_fails_fast() {
    let scratch = Scratch::new("stale-fast");
    let sock = scratch.path("s.sock");
    // A leftover socket file from a dead server: bind then close the
    // listener without unlinking.
    match bind_or_probe(&sock).unwrap() {
        BindOutcome::Bound(l) => drop(l),
        BindOutcome::AlreadyRunning => panic!("fresh path must bind"),
    }
    assert!(sock.exists(), "socket file must survive listener drop");
    // Converge to a fast refuse. macOS AF_UNIX has a brief teardown window
    // after the listener closes where one connect can still land on a residual
    // backlog slot; the invariant is that the socket QUICKLY settles to refused
    // and never hangs, not that the very first connect refuses.
    let start = std::time::Instant::now();
    let mut last = None;
    while start.elapsed() < Duration::from_secs(2) {
        match connect_unix_timeout(&sock, Duration::from_secs(1)) {
            Ok(residual) => {
                drop(residual); // in-flight teardown accept; retry
                continue;
            }
            Err(e) => {
                last = Some(e);
                break;
            }
        }
    }
    let err = last.expect("dead socket must settle to refused, never hang");
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::ConnectionRefused,
        "got: {err}"
    );
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[test]
fn connect_timeout_missing_path_fails_fast() {
    let scratch = Scratch::new("missing");
    let sock = scratch.path("never-created.sock");
    let err =
        connect_unix_timeout(&sock, Duration::from_secs(1)).expect_err("missing path must fail");
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound, "got: {err}");
}

#[test]
fn connect_timeout_live_listener_connects() {
    let scratch = Scratch::new("live-connect");
    let sock = scratch.path("s.sock");
    let _l = match bind_or_probe(&sock).unwrap() {
        BindOutcome::Bound(l) => l,
        BindOutcome::AlreadyRunning => panic!("fresh path must bind"),
    };
    connect_unix_timeout(&sock, Duration::from_secs(1)).expect("live listener must accept");
}

#[test]
fn connect_timeout_sets_close_on_exec() {
    // Regression guard: the raw libc socket must carry FD_CLOEXEC (std's
    // connect does), else the mux fd leaks into every child the client spawns.
    use std::os::unix::io::AsRawFd;
    let scratch = Scratch::new("cloexec");
    let sock = scratch.path("s.sock");
    let _l = match bind_or_probe(&sock).unwrap() {
        BindOutcome::Bound(l) => l,
        BindOutcome::AlreadyRunning => panic!("fresh path must bind"),
    };
    let stream = connect_unix_timeout(&sock, Duration::from_secs(1)).expect("must connect");
    let flags = unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_GETFD) };
    assert!(flags >= 0, "F_GETFD failed");
    assert_ne!(
        flags & libc::FD_CLOEXEC,
        0,
        "connect_unix_timeout must set FD_CLOEXEC"
    );
}

#[test]
fn proto_wedged_server_reads_alive_not_clobbered() {
    // A wedged-but-live server must read as AlreadyRunning at bind time WHEN the
    // platform surfaces the wedge as a connect TIMEOUT (Linux): unlinking its
    // socket would orphan it (running, unreachable by name). Where the wedge
    // surfaces as ConnectionRefused (macOS full/zero backlog), the server is
    // indistinguishable from dead and the clobber is the accepted, pre-existing
    // behavior - assert that explicitly rather than skip.
    let scratch = Scratch::new("wedged-bind");
    let sock = scratch.path("s.sock");
    let wedged = WedgedServer::new(&sock);
    if wedged.kind == WedgeKind::Refused {
        match bind_or_probe(&sock).unwrap() {
            // Refused reads as dead: the stale-socket takeover is correct here.
            BindOutcome::Bound(_) => {}
            BindOutcome::AlreadyRunning => {
                panic!("refused wedge should read as dead and be taken over")
            }
        }
        return;
    }
    match bind_or_probe(&sock).unwrap() {
        BindOutcome::AlreadyRunning => {}
        BindOutcome::Bound(_) => panic!("wedged-but-live socket was clobbered"),
    }
    assert!(
        sock.exists(),
        "wedged server's socket must survive the probe"
    );
}
