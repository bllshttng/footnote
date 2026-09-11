//! The keeper re-adoption test family: adoption at the socket stem's
//! birth id, the fresh-id fallback, and the dead-socket unlink.
//! Moved verbatim out of server.rs (file budget shrink). Parent helpers
//! resolve through the glob.
use super::*;
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
