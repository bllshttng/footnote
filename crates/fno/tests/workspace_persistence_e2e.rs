//! Workspace-persistence symptom repros (x-caef): rename, rearrange, and
//! deleted-workspaces-return, each driven on the REAL surface (a real server
//! binary over its socket, real PTYs) and each crossing a server restart,
//! which is the moment the operator loses work. Written red-first: each test
//! names the operator-visible symptom it pins, so a regression reads as the
//! symptom coming back rather than as an abstract assertion failing.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::sync::Mutex;
use std::time::Duration;

use common::{
    connect_with_retry, spawn_server, FakeClient, Scratch, ServerProc, ServerTermination,
};
use fno::proto::Command;

/// Same module-local serialization gate as `persistence.rs`: these tests own
/// real PTYs + Unix sockets, and parallel runs contend for the runner's CPU.
static PTY_GATE: Mutex<()> = Mutex::new(());

/// Attach a fresh client once the server is accepting.
fn attach_client(scratch: &Scratch) -> FakeClient {
    let _ = connect_with_retry(&scratch.main_sock());
    FakeClient::attach(&scratch.main_sock(), 30, 100, &scratch.home_cwd())
}

/// The operator's symptoms share this skeleton: drive the workspace live,
/// kill the server (what "ending fno" does), restart, read what came back.
struct Restarted {
    _server: ServerProc,
    client: FakeClient,
    _old_server: ServerTermination,
}

fn restart(scratch: &Scratch, incumbent: ServerProc) -> Restarted {
    let termination = incumbent.terminate_and_wait();
    let server = spawn_server(&scratch.main_sock(), &[]);
    let client = attach_client(scratch);
    Restarted {
        _server: server,
        client,
        _old_server: termination,
    }
}

#[test]
fn old_server_reaped_before_rebind_probe() {
    let _g = PTY_GATE.lock().unwrap_or_else(|e| e.into_inner());
    let scratch = Scratch::new("reap-probe");
    let incumbent = spawn_server(&scratch.main_sock(), &[]);
    // Wait for the incumbent to be ACCEPTING before signalling it. spawn_server
    // returns as soon as the child is forked, and a SIGTERM that lands before
    // the handler is installed takes the default action: the process dies, the
    // 3s grace never elapses, `forced` still reads false, and the probe reports
    // a graceful stop that never happened. Every other test in this file waits
    // here first.
    let _up = connect_with_retry(&scratch.main_sock());
    let termination = incumbent.terminate_and_wait();

    let mut replacement = spawn_server(&scratch.main_sock(), &[]);

    // The stress harness greps the line below as this test's whole verdict, so
    // the line must not be reachable when the thing it names did not happen.
    // Assert first, print second.
    //
    // Two assertions that used to stand here are gone on purpose.
    // `status.signal().is_some() || status.code().is_some()` is a tautology for
    // any collected status on Unix: it reads as a "was never collected" check
    // that cannot fire. `!termination.forced` was worse than useless here -
    // it turns a slow-but-correct graceful stop on a loaded runner into a
    // failed stress trial, which is noise in the exact number this harness
    // exists to measure.
    //
    // What is left can actually fail: the replacement must still be running,
    // and it must be accepting on the socket the incumbent held.
    assert!(
        replacement
            .0
            .try_wait()
            .expect("replacement server status")
            .is_none(),
        "replacement exited instead of rebinding {}",
        scratch.main_sock().display()
    );
    // Panics on its own 10s budget if the replacement never accepts.
    let _accepted = connect_with_retry(&scratch.main_sock());

    println!(
        "old_server_reaped_before_rebind old_pid={} new_pid={} socket={} status={:?}",
        termination.pid,
        replacement.0.id(),
        scratch.main_sock().display(),
        termination.status,
    );
}

/// The squad id currently named `name`, from the last absorbed layout.
fn squad_id(c: &FakeClient, name: &str) -> u64 {
    c.layout
        .as_ref()
        .and_then(|l| l.squads.iter().find(|s| s.name == name))
        .unwrap_or_else(|| panic!("no squad named {name} in layout"))
        .id
}

#[test]
fn symptom_rename_survives_restart() {
    let _g = PTY_GATE.lock().unwrap_or_else(|e| e.into_inner());
    let scratch = Scratch::new("rename");
    let server = spawn_server(&scratch.main_sock(), &[]);
    let mut c = attach_client(&scratch);
    c.wait_layout(10, "squads appear", |l| !l.squads.is_empty());

    c.cmd(Command::NewSquad {
        name: "w1".into(),
        origin: Some(scratch.home_cwd()),
    });
    c.wait_layout(10, "workspace w1 appears", |l| {
        l.squads.iter().any(|s| s.name == "w1")
    });
    let sid = squad_id(&c, "w1");
    c.cmd(Command::RenameSquad {
        squad: sid,
        name: "w2".into(),
    });
    c.wait_layout(10, "rename lands", |l| {
        l.squads.iter().any(|s| s.name == "w2") && !l.squads.iter().any(|s| s.name == "w1")
    });
    c.detach();

    let mut r = restart(&scratch, server);
    // Restore materializes persisted squads on the first attach, after the
    // initial layout: wait for the renamed workspace BY NAME so a slow
    // restore is never misread as a lost one.
    r.client.wait_layout(15, "renamed workspace restores", |l| {
        l.squads.iter().any(|s| s.name == "w2")
    });
    let names: Vec<&str> = r
        .client
        .layout
        .as_ref()
        .unwrap()
        .squads
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        names.contains(&"w2"),
        "renamed workspace came back as {names:?}"
    );
    assert!(
        !names.contains(&"w1"),
        "the pre-rename name resurrected alongside the rename: {names:?}"
    );
}

#[test]
fn symptom_hand_split_survives_restart() {
    let _g = PTY_GATE.lock().unwrap_or_else(|e| e.into_inner());
    let scratch = Scratch::new("split");
    let server = spawn_server(&scratch.main_sock(), &[]);
    let mut c = attach_client(&scratch);
    c.wait_layout(10, "squads appear", |l| !l.squads.is_empty());

    c.cmd(Command::NewSquad {
        name: "w".into(),
        origin: Some(scratch.home_cwd()),
    });
    c.wait_layout(10, "workspace w appears", |l| {
        l.squads.iter().any(|s| s.name == "w")
    });
    // A hand split (the prefix-key mutation), the exact topology the template
    // lane never captures: two panes in ONE tab.
    c.cmd(Command::SplitH);
    c.wait_layout(10, "split lands", |l| {
        l.squads
            .iter()
            .any(|s| s.name == "w" && s.tabs.len() == 1 && s.panes == 2)
    });
    c.detach();

    let mut r = restart(&scratch, server);
    // Wait for the SHAPE, not just the name, exactly as the pre-restart wait
    // ten lines up does. Restore materializes a squad and its panes in more
    // than one step, so a predicate satisfied by the name alone returns while
    // the second pane is still arriving, and the assertions below then read a
    // half-built layout and report one pane. Waiting on a weaker condition
    // than the one being asserted is the whole defect: the assertions stay so
    // a genuine topology loss still names what it found.
    r.client
        .wait_layout(15, "workspace w restores with its split", |l| {
            l.squads
                .iter()
                .any(|s| s.name == "w" && s.tabs.len() == 1 && s.panes == 2)
        });
    let w = r
        .client
        .layout
        .as_ref()
        .unwrap()
        .squads
        .iter()
        .find(|s| s.name == "w")
        .unwrap()
        .clone();
    assert_eq!(
        w.tabs.len(),
        1,
        "the split tab must come back as one tab, not one tab per pane"
    );
    assert_eq!(
        w.panes, 2,
        "the split topology must survive the restart (found {} panes)",
        w.panes
    );
}

#[test]
fn symptom_removed_workspace_stays_removed() {
    let _g = PTY_GATE.lock().unwrap_or_else(|e| e.into_inner());
    let scratch = Scratch::new("removed");
    let server = spawn_server(&scratch.main_sock(), &[]);
    let mut c = attach_client(&scratch);
    c.wait_layout(10, "squads appear", |l| !l.squads.is_empty());

    c.cmd(Command::NewSquad {
        name: "w".into(),
        origin: Some(scratch.home_cwd()),
    });
    c.wait_layout(10, "workspace w appears", |l| {
        l.squads.iter().any(|s| s.name == "w")
    });
    let sid = squad_id(&c, "w");
    c.cmd(Command::RemoveSquad(sid));
    c.wait_layout(10, "workspace w removed", |l| {
        !l.squads.iter().any(|s| s.name == "w")
    });
    c.detach();

    let mut r = restart(&scratch, server);
    r.client
        .wait_layout(10, "squads appear", |l| !l.squads.is_empty());
    // Give a would-be resurrection the same window the other repros give the
    // legitimate restore, so this never passes by reading too early.
    r.client.pump(Duration::from_secs(3));
    let names: Vec<&str> = r
        .client
        .layout
        .as_ref()
        .unwrap()
        .squads
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        !names.contains(&"w"),
        "a removed workspace resurrected at restart: {names:?}"
    );
}

#[test]
fn symptom_stale_live_row_does_not_respawn_a_dead_worker() {
    let _g = PTY_GATE.lock().unwrap_or_else(|e| e.into_inner());
    let scratch = Scratch::new("stale-live");
    // A `claude` stub on PATH whose only job is to leave markers when the
    // server spawns it: `boot` on ANY spawn (the positive control proving the
    // instrument reaches the target) and `attach` specifically on the
    // `claude attach <id>` respawn the bug produces. The fix is proven by
    // `attach` staying absent WITH `boot` reachable - an absence alone cannot
    // tell "not spawned" from "stub never ran".
    let bin = scratch.0.join("stubbin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(
        bin.join("claude"),
        "#!/bin/sh\necho boot >> \"$STUB_MARKER\"\nif [ \"$1\" = attach ]; then echo attach >> \"$STUB_MARKER\"; fi\nsleep 60\n",
    )
    .unwrap();
    std::fs::set_permissions(bin.join("claude"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let marker = scratch.0.join("marker");
    let _ = std::fs::remove_file(&marker);
    let path_with_stub = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    // A pid that is PROVABLY dead: reap a real short-lived child.
    let dead_pid = {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    };
    // The stale-live lie: the registry row claims a non-terminal status and a
    // claude attach id, but its pid is gone - exactly what a machine restart
    // leaves on disk, since a reboot writes nothing to the registry.
    let agents_home = scratch.0.join("iso-agents");
    std::fs::create_dir_all(&agents_home).unwrap();
    std::fs::write(
        agents_home.join("registry.json"),
        format!(
            r#"{{"agents":[{{"name":"ghost","cwd":"{}","status":"working","harness":"claude","short_id":"deadbeef","pid":{dead_pid},"pid_start_time":99887766}}]}}"#,
            scratch.home_cwd()
        ),
    )
    .unwrap();
    std::fs::write(
        agents_home.join("squads.json"),
        format!(
            r#"{{"version":1,"squads":[{{"name":"w","key":"","origins":["{}"],"members":[{{"attach_id":"deadbeef","tombstone":false}}],"created_at":"2026-08-14T00:00:00Z"}}]}}"#,
            scratch.home_cwd()
        ),
    )
    .unwrap();

    let _server = spawn_server(
        &scratch.main_sock(),
        &[
            ("PATH", path_with_stub.as_str()),
            ("STUB_MARKER", marker.to_str().unwrap()),
        ],
    );
    let mut c = attach_client(&scratch);
    // Restore runs at first attach; give the spawn it should NOT make a
    // moment to have happened.
    c.pump(Duration::from_secs(3));
    c.wait_layout(10, "workspace w appears", |l| {
        l.squads.iter().any(|s| s.name == "w")
    });

    // Positive control: the stub is reachable through the server's own spawn
    // path (`pane run`), so an absent `attach` means "not spawned", never
    // "instrument broken".
    let mut probe = scratch.command();
    probe
        .env("PATH", &path_with_stub)
        .env("STUB_MARKER", marker.to_str().unwrap())
        .args(["mux", "pane", "run", "--", "claude", "probe"]);
    let out = probe.output().unwrap();
    assert!(
        out.status.success(),
        "pane-run probe failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    c.pump(Duration::from_secs(2));

    let text = std::fs::read_to_string(&marker).unwrap_or_default();
    assert!(
        text.contains("boot"),
        "stub never ran: control failed ({text:?})"
    );
    assert!(
        !text.contains("attach"),
        "restore spawned `claude attach deadbeef` for a row whose pid ({dead_pid}) is dead: the stale-live registry lie respawning a dead worker"
    );
    c.detach();
}
