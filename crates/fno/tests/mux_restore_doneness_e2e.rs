//! x-9052 + x-2990 marker e2e: restore lands at in-flight members plus the
//! shells actually open, and the store converges instead of accruing. SIGKILL
//! the server, restart, assert the counts and the one-line receipt. Hermetic
//! via the common harness (isolated HOME / registry / graph / store).

mod common;

use common::{spawn_server, FakeClient, Scratch};

use std::path::Path;
use std::time::{Duration, Instant};

/// Poll until `sock` exists (the server bound its listener).
fn wait_socket_present(sock: &Path, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if sock.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    sock.exists()
}

/// Poll until no process commands the server socket path.
fn wait_server_gone(sock: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let out = std::process::Command::new("pgrep")
            .arg("-f")
            .arg(sock.to_str().unwrap())
            .output()
            .unwrap();
        if out.stdout.is_empty() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// SIGKILL every server bound to `sock`'s path (persistence.rs's shape).
fn kill_server(sock: &Path) {
    let _ = std::process::Command::new("pkill")
        .arg("-9")
        .arg("-f")
        .arg(sock.to_str().unwrap())
        .status();
}

/// The fixture: an isolated graph naming the done member's (harness, session)
/// pair on a done node, and a stored home-lane squad with two worker members
/// (done + in-flight) and three trees (done slot, in-flight slot, plain
/// shell).
fn write_fixture(iso: &Path) {
    let graph = r#"{"entries":[{"id":"x-done","status":"done","sessions":[{"harness":"codex","session_id":"done-sess"}]}]}"#;
    std::fs::write(iso.join("iso-graph.json"), graph).unwrap();
    let home = iso.join("home");
    std::fs::create_dir_all(home.join(".fno")).unwrap();
    let origin = iso.join("repo");
    std::fs::create_dir_all(&origin).unwrap();
    let origin_str = origin.to_string_lossy();
    let squad = r#"{"version": 1, "squads":[{
            "name": "",
            "key": "",
            "origins": ["__ORIGIN__"],
            "members": [
                {"attach_id": "", "worker": "t-done", "harness": "codex", "harness_session_id": "done-sess"},
                {"attach_id": "", "worker": "t-live", "harness": "codex", "harness_session_id": "live-sess"}
            ],
            "tab_trees": [
                {"tree": {"slot": "s0"}, "slots": [{"name": "s0", "binding": {"fno": "worker:codex:done-sess"}}]},
                {"tree": {"slot": "s0"}, "slots": [{"name": "s0", "binding": {"fno": "worker:codex:live-sess"}}]},
                {"tree": {"slot": "s0"}, "slots": [{"name": "s0", "binding": "shell"}]}
            ]
        }]}"#;
    let squad = squad.replace("__ORIGIN__", &origin_str);
    std::fs::create_dir_all(iso.join("iso-agents")).unwrap();
    std::fs::write(iso.join("iso-agents").join("squads.json"), squad).unwrap();
}

#[test]
fn sigkill_lands_at_inflight_plus_shells() {
    let s = Scratch::new("mux-doneness");
    let sock = s.main_sock();
    write_fixture(&s.0);
    let _server = spawn_server(&sock, &[]);
    assert!(
        wait_socket_present(&sock, 10),
        "server never bound its socket"
    );
    // Attach triggers restore: the done member is skipped (no pane, named
    // once), the in-flight member gets its pane, the shell tree is consumed
    // by the fresh attach shell's claim.
    let mut client = FakeClient::attach(&sock, 24, 80, s.0.join("repo").to_str().unwrap());
    client.wait(15, "the done-member receipt", |c| {
        c.notices
            .iter()
            .any(|n| n.contains("skipped 1 done worker pane(s)") && n.contains("t-done"))
            .then_some(())
    });
    let snap = client.wait_layout(15, "restored layout", |l| {
        l.squads.iter().map(|sq| sq.tabs.len()).sum::<usize>() >= 2
    });
    let tabs: usize = snap.squads.iter().map(|sq| sq.tabs.len()).sum();
    let joined: String = client.notices.join("\n");
    assert_eq!(
        tabs, 2,
        "in-flight member tab + fresh attach shell: {joined}"
    );
    kill_server(&sock);
    wait_server_gone(&sock);
    // SIGKILL unlinks nothing: drop the stale socket file so the restart
    // can bind.
    let _ = std::fs::remove_file(&sock);

    // The store holds the removal now: two trees (fresh shell + in-flight).
    let store: String =
        std::fs::read_to_string(s.0.join("iso-agents").join("squads.json")).unwrap();
    let tree_count = store.matches("\"tree\"").count();
    assert_eq!(tree_count, 2, "the removal was written: {store}");

    // Restart, attach, restore again: no accretion, the same landing.
    let _server2 = spawn_server(&sock, &[]);
    assert!(
        wait_socket_present(&sock, 10),
        "second server never bound its socket"
    );
    let mut client2 = FakeClient::attach(&sock, 24, 80, s.0.join("repo").to_str().unwrap());
    client2.wait(15, "second receipt", |c| {
        c.notices
            .iter()
            .any(|n| n.contains("skipped 1 done worker pane(s)"))
            .then_some(())
    });
    let snap2 = client2.wait_layout(15, "second restore", |l| {
        l.squads.iter().map(|sq| sq.tabs.len()).sum::<usize>() >= 2
    });
    let tabs2: usize = snap2.squads.iter().map(|sq| sq.tabs.len()).sum();
    let joined2: String = client2.notices.join("\n");
    assert!(
        joined2.contains("skipped 1 done worker pane(s)"),
        "the receipt still names the done member: {joined2}"
    );
    assert_eq!(
        tabs2, 2,
        "converged: no accretion across restarts: {joined2}"
    );
}
