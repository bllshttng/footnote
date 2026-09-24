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
    let sock = dir.join("kt-9.sock");
    std::fs::write(&sock, b"").unwrap(); // a dead keeper's leftover

    let mut core = empty_core();
    core.session_name = "kt".to_string();
    core.keeper_readopt();

    assert!(
        !sock.exists(),
        "a socket with nothing behind it is removed, not waited on"
    );
    assert!(
        core.panes.is_empty(),
        "no pane is minted for a stale socket"
    );
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
