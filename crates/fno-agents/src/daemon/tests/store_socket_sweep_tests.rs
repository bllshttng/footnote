//! Which store sockets and orphaned seat locks the keeper sweep may unlink.
//! Extracted from daemon.rs's tests mod: daemon.rs is over file budget and
//! may only shrink; the sweep itself moved beside this file.

use super::keeper_sweep::keeper_sweep_home;
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
    std::fs::create_dir_all(&temp_root).unwrap();

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

    // The dead hashed name comes from the naming authority itself so the
    // fixture cannot drift from it again: store_socket_for names files
    // <16hex>.sock under the uid-keyed temp root.
    let long_graph = std::path::PathBuf::from(format!("/tmp/{}.out", "g".repeat(120)));
    let hashed_name = crate::graph_keeper::store_socket_for(&long_graph)
        .file_name()
        .unwrap()
        .to_owned();
    let dead_hashed = temp_root.join(&hashed_name);
    // The hashed root is ours by construction, so its litter can also be
    // a bare file (ENOTSOCK) - covered on this fixture.
    std::fs::write(&dead_hashed, b"").unwrap();

    // A live listener on a real hashed name in the temp root: never
    // unlinked, still accepting after the sweep.
    let live_hashed = temp_root.join("0123456789abcdef.sock");
    let live_listener = std::os::unix::net::UnixListener::bind(&live_hashed).unwrap();

    // An orphaned seat lock: <x>.sock.lock with no <x>.sock and no holder.
    let orphan_lock = temp_root.join("1111222233334444.sock.lock");
    std::fs::write(&orphan_lock, b"").unwrap();

    // A seat lock another open file still holds: stays (WouldBlock).
    let held_lock = temp_root.join("5555666677778888.sock.lock");
    let holder = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&held_lock)
        .unwrap();
    holder.try_lock().unwrap();

    // The one-shot return count is not asserted exactly: a probe that
    // errors unreadably (interrupt, resource pressure mid-suite) is
    // deliberately conservative in production - the socket stays and the
    // next sweep re-decides. What must hold on EVERY pass is the safety
    // invariant (shielded, live, and held-lock untouched), and
    // provably-dead litter must be gone after bounded passes.
    let mut passes = 0;
    while passes < 10 {
        let _ = crate::daemon::store_socket_sweep::store_socket_sweep_in(
            &home,
            temp_root.clone(),
            &emitter,
        );
        passes += 1;
        assert!(
            shielded_sock.exists(),
            "a shielded sibling is never unlinked"
        );
        assert!(
            live_hashed.exists(),
            "a live hashed listener is never unlinked"
        );
        std::os::unix::net::UnixStream::connect(&live_hashed)
            .expect("a live hashed listener still accepts");
        assert!(held_lock.exists(), "a held seat lock is never unlinked");
        if !orphan_sock.exists() && !dead_hashed.exists() && !orphan_lock.exists() {
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
        "hashed litter with the real <16hex>.sock name is not unlinked ({passes} passes)"
    );
    assert!(
        !orphan_lock.exists(),
        "an orphaned seat lock is not unlinked ({passes} passes)"
    );
    assert!(shielded_graph.exists(), "non-socket files are untouched");
    assert!(
        crate::events::committed_journal_text(&home.events_jsonl())
            .contains("store_seat_lock_unlinked"),
        "the sweep emits store_seat_lock_unlinked for the orphaned lock"
    );
    drop(live_listener);
    drop(holder);
    let _ = std::fs::remove_file(&shielded_sock);
    let _ = std::fs::remove_file(&shielded_graph);
}
