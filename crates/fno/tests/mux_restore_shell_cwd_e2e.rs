//! x-5baf marker e2e: a shell tab's captured cwd survives a SIGKILL restart.
//! A bare `cd` never marks topology dirty (x-9052's funnel is a layout
//! mutation, not shell I/O), so `RenameTab` forces the same synchronous
//! `persist_squad` -> `persist_tab_trees` write the debounced tick would
//! eventually take, reading the shell's CURRENT cwd. Hermetic via the common
//! FakeClient/spawn_server harness (isolated HOME / registry / graph /
//! store).
//!
//! Reads the cwd back from the restored PANE itself (a live `pwd`), never
//! from the store: a store-level assertion would only prove capture wrote
//! the field, not that restore actually spawned there.

mod common;

use common::{spawn_server, FakeClient, Scratch};
use fno::proto::Command;

use std::path::Path;
use std::time::{Duration, Instant};

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

fn kill_server(sock: &Path) {
    let _ = std::process::Command::new("pkill")
        .arg("-9")
        .arg("-f")
        .arg(sock.to_str().unwrap())
        .status();
}

/// Attach, `cd` the lone shell pane into `dir`, then force the churn a
/// topology mutation would cause (`RenameTab` re-persists inline, no need to
/// wait out the 2s debounce). Returns the pane id so the caller can read it
/// back after a restart.
fn attach_cd_and_force_persist(sock: &Path, origin: &str, dir: &Path) -> u64 {
    let mut client = FakeClient::attach(sock, 24, 240, origin);
    let layout = client.wait_layout(15, "one attach shell", |l| l.panes.len() == 1);
    let pane = layout.panes[0].0;
    let tab = layout.squads[0].tabs[0].id;
    client.wait_prompt(pane);
    client.input(format!("cd {}\r", dir.display()).as_bytes());
    client.wait_prompt(pane);
    client.cmd(Command::RenameTab {
        tab,
        name: "cwd-marker".into(),
    });
    client.wait(15, "the rename's persist to land", |c| {
        c.layout
            .as_ref()
            .and_then(|l| l.squads.first())
            .and_then(|sq| sq.tabs.first())
            .filter(|t| t.name == "cwd-marker")
            .map(|_| ())
    });
    pane
}

/// Restore mints a fresh, unnamed attach shell ALONGSIDE the restored
/// "cwd-marker" tab: naming the tab took it out of the unnamed-shell claim
/// x-9052 established, so it is never the tab the server focuses by default.
/// Wait for both tabs to land, then explicitly focus the marker pane and
/// wait for its shell to be ready for input.
fn focus_restored_marker_tab(client: &mut FakeClient) -> u64 {
    let layout = client.wait_layout(15, "restore lands the marker tab", |l| {
        l.squads
            .first()
            .is_some_and(|sq| sq.tabs.iter().any(|t| t.name == "cwd-marker"))
    });
    let pane = layout.squads[0]
        .tabs
        .iter()
        .find(|t| t.name == "cwd-marker")
        .and_then(|t| t.panes.first())
        .expect("cwd-marker tab has a pane")
        .id;
    client.cmd(Command::FocusPane(pane));
    client.wait_layout(15, "focus moves to the marker pane", |l| l.focus == pane);
    client.wait_prompt(pane);
    pane
}

#[test]
fn restored_shell_tab_lands_in_its_captured_cwd() {
    // AC5-HP: a shell pane's cwd at capture is where restore reopens it, not
    // the squad root. The marker is the directory's own basename - only a
    // shell that actually landed there can print it back.
    let s = Scratch::new("shell-cwd");
    let sock = s.main_sock();
    let origin = s.0.join("repo");
    std::fs::create_dir_all(&origin).unwrap();
    let marker_dir = s.0.join("x5baf-shell-cwd-marker");
    std::fs::create_dir_all(&marker_dir).unwrap();
    // Canonicalize once: macOS resolves /tmp -> /private/tmp at the kernel,
    // and `process_cwd` reads the kernel's own answer - the expectation must
    // be built from that same resolved path or the comparison never matches.
    let origin = std::fs::canonicalize(&origin).unwrap();
    let marker_dir = std::fs::canonicalize(&marker_dir).unwrap();

    let _server = spawn_server(&sock, &[("SHELL", "/bin/sh")]);
    assert!(
        wait_socket_present(&sock, 10),
        "server never bound its socket"
    );
    attach_cd_and_force_persist(&sock, origin.to_str().unwrap(), &marker_dir);

    kill_server(&sock);
    wait_server_gone(&sock);
    // SIGKILL unlinks nothing: drop the stale socket file so the restart can
    // bind.
    let _ = std::fs::remove_file(&sock);

    let _server2 = spawn_server(&sock, &[("SHELL", "/bin/sh")]);
    assert!(
        wait_socket_present(&sock, 10),
        "second server never bound its socket"
    );
    let mut client2 = FakeClient::attach(&sock, 24, 240, origin.to_str().unwrap());
    let pane2 = focus_restored_marker_tab(&mut client2);

    client2.input(b"pwd\r");
    client2.wait_pane_text(15, pane2, |t| {
        t.lines().any(|l| l.trim() == marker_dir.to_string_lossy())
    });
}

#[test]
fn restored_shell_tab_falls_back_and_notices_a_vanished_cwd() {
    // AC6-EDGE: a captured cwd that no longer exists degrades to the squad
    // root, with one notice naming both paths - never a silent landing.
    let s = Scratch::new("shell-cwd-gone");
    let sock = s.main_sock();
    let origin = s.0.join("repo");
    std::fs::create_dir_all(&origin).unwrap();
    let marker_dir = s.0.join("x5baf-shell-cwd-gone-marker");
    std::fs::create_dir_all(&marker_dir).unwrap();
    let origin = std::fs::canonicalize(&origin).unwrap();
    let marker_dir = std::fs::canonicalize(&marker_dir).unwrap();

    let _server = spawn_server(&sock, &[("SHELL", "/bin/sh")]);
    assert!(
        wait_socket_present(&sock, 10),
        "server never bound its socket"
    );
    attach_cd_and_force_persist(&sock, origin.to_str().unwrap(), &marker_dir);

    kill_server(&sock);
    wait_server_gone(&sock);
    let _ = std::fs::remove_file(&sock);
    // The directory the shell was captured in is gone by the time restore
    // runs.
    std::fs::remove_dir_all(&marker_dir).unwrap();

    let _server2 = spawn_server(&sock, &[("SHELL", "/bin/sh")]);
    assert!(
        wait_socket_present(&sock, 10),
        "second server never bound its socket"
    );
    let mut client2 = FakeClient::attach(&sock, 24, 240, origin.to_str().unwrap());
    let pane2 = focus_restored_marker_tab(&mut client2);

    client2.input(b"pwd\r");
    client2.wait_pane_text(15, pane2, |t| {
        t.lines().any(|l| l.trim() == origin.to_string_lossy())
    });
    let marker_str = marker_dir.to_string_lossy().to_string();
    client2.wait(15, "the vanished-path notice", |c| {
        c.notices
            .iter()
            .any(|n| n.contains(&marker_str) && n.contains("gone"))
            .then_some(())
    });
}
