//! The keeper re-adoption test family: adoption at the socket stem's
//! birth id, the fresh-id fallback, and the dead-socket unlink.
//! Moved verbatim out of server.rs (file budget shrink). Parent helpers
//! resolve through the glob.
use super::*;

/// The sibling keeper binary when both crates are built side by side
/// (CI and this repo's dev flow both do). `None` = skip loudly rather
/// than fake a green: these tests assert REAL process survival.
fn keeper_test_bin() -> Option<std::path::PathBuf> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fno-agents/target/debug/fno-agents-worker");
    p.exists().then(|| p.canonicalize().unwrap_or(p))
}

struct KeeperProcess(std::process::Child);

impl Drop for KeeperProcess {
    fn drop(&mut self) {
        // SAFETY: SIGKILL to a process this test spawned.
        unsafe {
            libc::kill(self.0.id() as libc::pid_t, libc::SIGKILL);
        }
        let _ = self.0.wait();
    }
}

fn spawn_keeper_for_test(
    bin: &std::path::Path,
    sock: &std::path::Path,
    provider: &[&str],
) -> KeeperProcess {
    let mut cmd = std::process::Command::new(bin);
    cmd.args([
        "--pane",
        "--sock",
        &sock.to_string_lossy(),
        "--session",
        "kt",
        "--pane-key",
        "3",
        "--cwd",
        "/tmp",
        "--",
    ]);
    cmd.args(provider);
    // The keeper outlives the server by design, so without an owner
    // it survives this test run as a ppid-1 orphan. The test binary IS the
    // owner; the watchdog inside the keeper reaps it when the run ends.
    cmd.envs(crate::test_owner::self_owner_env());
    let child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("keeper spawns");
    KeeperProcess(child)
}
#[test]
fn keeper_readopt_adopts_the_surviving_child_and_binds_it_to_its_member() {
    let Some(bin) = keeper_test_bin() else {
        eprintln!(
            "SKIPPING keeper_readopt_adopts_the_surviving_child_and_binds_it_to_its_member: \
                 build crates/fno-agents first (no sibling fno-agents-worker binary)"
        );
        return;
    };
    let dir = crate::proto::mux_dir().join("panes");
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("kt-3.sock");
    let _ = std::fs::remove_file(&sock);
    // The provider argv carries the worker name exactly the mesh wrapper
    // carries it, so the re-adopt join runs the real parser.
    let keeper = spawn_keeper_for_test(
        &bin,
        &sock,
        &["env", "FNO_AGENT_SELF=t-keeper-worker", "sleep", "300"],
    );
    // The keeper binds asynchronously; the sweep scans what EXISTS, so
    // wait for the socket before sweeping (a real server start meets
    // keepers that are minutes old, never milliseconds).
    let bound = Instant::now();
    while !sock.exists() {
        assert!(
            bound.elapsed() < Duration::from_secs(10),
            "keeper never bound its socket"
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    let mut core = empty_core();
    core.session_name = "kt".to_string();
    core.keeper_readopt();

    assert_eq!(core.panes.len(), 1, "the live keeper became one pane");
    assert_eq!(
        core.keeper_adopted.len(),
        1,
        "the adoption is staged for restore"
    );
    let pane = core.keeper_adopted[0].pane;
    assert_eq!(
        pane, 3,
        "the pane is adopted at the id its socket stem carries"
    );
    assert!(
        !core.panes[&pane].unreconciled,
        "a reused birth id is reconciled"
    );
    let child_pid = core.keeper_adopted[0]
        .child_pid
        .expect("the adopt names the child pid");
    assert_ne!(
        child_pid,
        keeper.0.id(),
        "the recorded pid is the CHILD's, never the keeper's"
    );
    assert_eq!(
        core.panes[&pane].pty.child_pid(),
        Some(child_pid),
        "pane ls will read the child pid"
    );
    assert_eq!(
        core.panes[&pane].name.as_deref(),
        Some("t-keeper-worker"),
        "the pane is named from the argv's worker token"
    );

    // The restore-side join: the member binds by the worker name read
    // back out of the adopted argv, once, and a stranger never binds.
    let member = crate::squad_store::StoredMember {
        attach_id: String::new(),
        tombstone: false,
        tombstone_reason: None,
        detached: false,
        tab_name: None,
        cwd: Some("/tmp".into()),
        worker: Some("t-keeper-worker".into()),
        harness: Some("claude".into()),
        harness_session_id: Some("sess-1".into()),
        pane_id: None,
    };
    assert_eq!(
        core.take_adopted_for_member(&member),
        Some(pane),
        "the member's pane is the adopted one"
    );
    assert_eq!(
        core.take_adopted_for_member(&member),
        None,
        "the binding is once-only"
    );
}

#[test]
fn keeper_readopt_adopts_at_a_fresh_unreconciled_id_when_the_birth_id_is_taken() {
    let Some(bin) = keeper_test_bin() else {
        eprintln!(
                "SKIPPING keeper_readopt_adopts_at_a_fresh_unreconciled_id_when_the_birth_id_is_taken: \
                 build crates/fno-agents first (no sibling fno-agents-worker binary)"
            );
        return;
    };
    let dir = crate::proto::mux_dir().join("panes");
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("ku-3.sock");
    let _ = std::fs::remove_file(&sock);
    let _keeper = spawn_keeper_for_test(
        &bin,
        &sock,
        &["env", "FNO_AGENT_SELF=t-keeper-taken", "sleep", "300"],
    );
    let bound = Instant::now();
    while !sock.exists() {
        assert!(
            bound.elapsed() < Duration::from_secs(10),
            "keeper never bound its socket"
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    let mut core = empty_core();
    core.session_name = "ku".to_string();
    core.shells = vec!["/bin/cat".into()];
    core.next_pane_id = 3;
    let squatter = core.spawn_pane(24, 80, "/tmp").unwrap();
    assert_eq!(squatter, 3, "the birth id is occupied before the sweep");
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);
    core.keeper_readopt();

    assert_eq!(core.keeper_adopted.len(), 1, "the live keeper is adopted");
    let pane = core.keeper_adopted[0].pane;
    assert_ne!(pane, squatter, "an occupied birth id is never reused");
    assert!(
        core.panes[&pane].unreconciled,
        "a fresh-id adoption is marked unreconciled"
    );
    let notices = drain_notices(&mut rx).join("\n");
    assert!(
        notices.contains("ku-3.sock") && notices.contains("as unreconciled"),
        "the fallback names the socket and the outcome: {notices}"
    );
}

#[test]
fn keeper_readopt_unlinks_a_socket_with_no_live_keeper_and_names_it() {
    let dir = crate::proto::mux_dir().join("panes");
    std::fs::create_dir_all(&dir).unwrap();
    // Three stale sockets: the power-loss shape. Each must be refused at
    // once (stale-is-final), not retried for the handshake deadline, so the
    // whole sweep lands in well under a second where the retrying handshake
    // used to cost 5s per socket (x-dbdb AC1).
    let socks: Vec<_> = ["kt-7.sock", "kt-8.sock", "kt-9.sock"]
        .iter()
        .map(|n| dir.join(n))
        .collect();
    for sock in &socks {
        std::fs::write(sock, b"").unwrap(); // a dead keeper's leftover
    }

    let mut core = empty_core();
    core.session_name = "kt".to_string();
    let started = std::time::Instant::now();
    core.keeper_readopt();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "three stale sockets must be refused at once, not retried: {:?}",
        started.elapsed()
    );

    for sock in &socks {
        assert!(
            !sock.exists(),
            "a socket with nothing behind it is removed, not waited on: {}",
            sock.display()
        );
    }
    assert!(
        core.panes.is_empty(),
        "no pane is minted for a stale socket"
    );
}

#[test]
fn keeper_handshake_is_bounded_and_names_the_keepers_death() {
    // x-dbdb AC2 + AC3: the handshake cannot park the server. The adopt
    // road against a listener that never answers Identify errors within the
    // deadline and names the socket; the spawn road against a keeper that
    // dies mid-handshake names the exit signal, never a bare os error.
    let dir = crate::proto::mux_dir().join("panes");
    std::fs::create_dir_all(&dir).unwrap();

    // Adopt road (AC2): a live listener that accepts and never answers.
    let wedged = dir.join("kz-2.sock");
    let _ = std::fs::remove_file(&wedged);
    let _listener = std::os::unix::net::UnixListener::bind(&wedged).unwrap();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (exit_tx, _exit_rx) = tokio::sync::mpsc::channel(4);
    let started = std::time::Instant::now();
    let err = crate::pty::adopt_keeper_socket(&wedged, 2, tx, exit_tx)
        .err()
        .expect("a keeper that never answers must error, not park the caller");
    assert!(
        started.elapsed() < std::time::Duration::from_millis(3500),
        "the wedged keeper must cost at most the 3s deadline: {:?}",
        started.elapsed()
    );
    assert!(
        err.contains("kz-2.sock"),
        "the error names the socket: {err}"
    );
    assert!(
        err.contains("did not finish the handshake"),
        "the error names the bound it hit: {err}"
    );
    let _ = std::fs::remove_file(&wedged);

    // Spawn road (AC3): a fake keeper binds, accepts one Identify byte,
    // then SIGKILLs itself, so the handshake fails after connect. If the
    // machine has no python3 the spawn road is skipped loudly.
    let python3 = std::process::Command::new("python3")
        .arg("-c")
        .arg("pass")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    if python3.is_err() {
        eprintln!("SKIPPING the spawn road: no python3 for the fake keeper");
        return;
    }
    let fake = dir.join("kz-fake-keeper.py");
    std::fs::write(
        &fake,
        "#!/usr/bin/env python3\nimport socket, os, signal, sys\nargs = sys.argv[1:]\nsock = args[args.index(\"--sock\") + 1]\ns = socket.socket(socket.AF_UNIX)\ns.bind(sock)\ns.listen(1)\nc, _ = s.accept()\nc.recv(1)\nos.kill(os.getpid(), signal.SIGKILL)\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let permit = crate::process_admission::admit_fallback().expect("test permit");
    let (tx2, _rx2) = tokio::sync::mpsc::channel(64);
    let (exit_tx2, _exit_rx2) = tokio::sync::mpsc::channel(4);
    let err = crate::pty::PtyShell::spawn_cmd_keeper_with_permit(
        &fake,
        &["sleep".to_string(), "1".to_string()],
        24,
        80,
        None,
        "kz",
        4,
        tx2,
        exit_tx2,
        permit,
    )
    .err()
    .expect("the keeper died mid-handshake; spawn must fail");
    let msg = err.to_string();
    assert!(msg.contains("keeper exited"), "names the death: {msg}");
    assert!(msg.contains("signal 9"), "names the signal: {msg}");
}

#[test]
fn take_adopted_for_slot_binds_by_birth_pane_id_once_only() {
    // The SHELL-slot restart join: the stored leaf's pane id (globally
    // monotonic, re-adopted at birth) binds the adoptee to its own leaf.
    // A stranger id never joins, and the join is once-only.
    let Some(bin) = keeper_test_bin() else {
        eprintln!(
            "SKIPPING take_adopted_for_slot_binds_by_birth_pane_id_once_only: \
                 build crates/fno-agents first (no sibling fno-agents-worker binary)"
        );
        return;
    };
    let dir = crate::proto::mux_dir().join("panes");
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("kt-5.sock");
    let _ = std::fs::remove_file(&sock);
    let _keeper = spawn_keeper_for_test(&bin, &sock, &["sleep", "300"]);
    let bound = Instant::now();
    while !sock.exists() {
        assert!(
            bound.elapsed() < Duration::from_secs(10),
            "keeper never bound its socket"
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    let mut core = empty_core();
    core.session_name = "kt".to_string();
    core.keeper_readopt();
    assert_eq!(core.keeper_adopted.len(), 1, "the keeper pane is staged");
    assert_eq!(core.keeper_adopted[0].pane, 5, "adopted at the birth id");

    assert_eq!(
        core.take_adopted_for_slot(9),
        None,
        "a stranger birth id never joins"
    );
    assert_eq!(
        core.take_adopted_for_slot(5),
        Some(5),
        "the stored leaf's birth id joins the adoptee"
    );
    assert_eq!(core.take_adopted_for_slot(5), None, "the join is once-only");
}

#[test]
fn keeper_survives_shutdown_sweep_and_plain_panes_do_not() {
    // The contract the future sigwait reaper must keep: a shutdown-shaped
    // sweep kills plain pane children and leaves keeper-hosted panes for
    // the next server to re-adopt. Deliberate close (reap_pane) is the
    // only path that kills a keeper pane.
    let mut core = empty_core();
    core.shells = vec!["/bin/sh".into()];
    let plain = core.spawn_pane(24, 80, "/tmp").expect("plain pane spawns");

    let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
    let keeper_pane = {
        let id = core.reserve_pane_id().unwrap();
        core.register_pane(
            id,
            PtyShell::Keeper(crate::pty::KeeperPty::for_test(b, Some(999_999))),
            24,
            80,
            None,
            None,
            "/tmp".into(),
            None,
            None,
            None,
            None,
            None,
            false,
        )
        .unwrap();
        id
    };

    core.kill_all_panes();

    // The plain child is dead (poll: SIGKILL is fast but not instant).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while core.panes[&plain].pty.is_child_alive() {
        assert!(
            std::time::Instant::now() < deadline,
            "the plain pane's child must die in the shutdown sweep"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // The keeper pane got NO kill: nothing arrives on its wire inside a
    // window far longer than the Local kill takes.
    std::thread::sleep(std::time::Duration::from_millis(300));
    let mut probe = a;
    use std::io::Read as _;
    probe
        .set_read_timeout(Some(std::time::Duration::from_millis(200)))
        .unwrap();
    let mut byte = [0u8; 1];
    assert!(
        probe.read(&mut byte).is_err(),
        "the shutdown sweep must never send a Kill frame to a keeper pane"
    );
    assert!(core.panes.contains_key(&keeper_pane));
}
