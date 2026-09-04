//! Which store sockets the keeper sweep may unlink. Extracted from
//! daemon.rs's tests mod: daemon.rs is over file budget and may only shrink.

use super::*;

#[test]
fn store_socket_sweep_unlinks_the_dead_and_leaves_the_live() {
    // Sibling sockets whose graph still lives stay for the client's
    // connect-before-bind; orphaned sockets (graph gone) and hashed-root
    // litter are unlinked; a live listener is left exactly as found.
    let home = keeper_sweep_home("storesock");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let state_root = home.root().parent().unwrap().to_path_buf();
    let temp_root = state_root.join("hashed");

    // A dead sibling whose graph still exists: NOT ours to unlink. A
    // client rebind is one connection away, and unlinking here is the
    // steal race this rule exists to close.
    let shielded_sock = state_root.join("graph.json.store.sock");
    let corpse = std::os::unix::net::UnixListener::bind(&shielded_sock).unwrap();
    drop(corpse);
    let shielded_graph = state_root.join("graph.json");
    std::fs::write(&shielded_graph, b"{}").unwrap();

    // An orphaned sibling: same dead shape, but its graph is gone, so no
    // keeper can ever be behind the path again.
    let orphan_sock = state_root.join("deleted-graph.json.store.sock");
    let corpse2 = std::os::unix::net::UnixListener::bind(&orphan_sock).unwrap();
    drop(corpse2);

    let dead_hashed = temp_root.join(".fno-store-abcdef1234567890.sock");
    std::fs::create_dir_all(&temp_root).unwrap();
    // The hashed root is ours by construction, so its litter can also be
    // a bare file (ENOTSOCK) - covered on this fixture.
    std::fs::write(&dead_hashed, b"").unwrap();

    // A live listener: a bound unix socket in the state root.
    let live_sock = state_root.join("other.store.sock");
    let listener = std::os::unix::net::UnixListener::bind(&live_sock).unwrap();

    // The one-shot return count is not asserted exactly: a probe that
    // errors unreadably (interrupt, resource pressure mid-suite) is
    // deliberately conservative in production - the socket stays and the
    // next sweep re-decides. What must hold on EVERY pass is the safety
    // invariant (shielded and live untouched), and provably-dead litter
    // must be gone after bounded passes.
    let mut passes = 0;
    while passes < 10 {
        let _ = store_socket_sweep_in(&home, temp_root.clone(), &emitter);
        passes += 1;
        assert!(
            shielded_sock.exists(),
            "a shielded sibling is never unlinked"
        );
        assert!(live_sock.exists(), "a live listener is never unlinked");
        if !orphan_sock.exists() && !dead_hashed.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !orphan_sock.exists(),
        "an orphaned socket is not unlinked ({passes} passes)"
    );
    assert!(
        !dead_hashed.exists(),
        "hashed litter is not unlinked ({passes} passes)"
    );
    assert!(shielded_graph.exists(), "non-socket files are untouched");
    drop(listener);
    let _ = std::fs::remove_file(&live_sock);
    let _ = std::fs::remove_file(&shielded_sock);
    let _ = std::fs::remove_file(&shielded_graph);
}
