//! Workspace-persistence symptom repros (x-caef): rename, rearrange, and
//! deleted-workspaces-return, each driven on the REAL surface (a real server
//! binary over its socket, real PTYs) and each crossing a server restart,
//! which is the moment the operator loses work. Written red-first: each test
//! names the operator-visible symptom it pins, so a regression reads as the
//! symptom coming back rather than as an abstract assertion failing.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use common::{
    connect_with_retry, spawn_server, FakeClient, Scratch, ServerProc, ServerTermination,
};
use fno::proto::{Command, PanePlacement};

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

fn kill_server(
    scratch: &Scratch,
    mut incumbent: ServerProc,
    client: &mut FakeClient,
) -> ServerTermination {
    let out = scratch
        .command()
        .args(["mux", "kill-server", "main"])
        .output()
        .expect("kill-server runs");
    assert!(
        out.status.success(),
        "kill-server failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    client.wait_killed(10, |c| c.byes.iter().any(|r| r.contains("killed")));
    let pid = incumbent.0.id();
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = incumbent.0.try_wait().expect("owned server status") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "server must exit after kill-server"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        !scratch.main_sock().exists(),
        "kill-server must unlink the server socket"
    );
    ServerTermination { pid, status }
}

fn drive_named_three_pane_layout(client: &mut FakeClient, scratch: &Scratch) {
    client.wait_layout(10, "squads appear", |l| !l.squads.is_empty());
    client.cmd(Command::NewSquad {
        name: "w".into(),
        origin: Some(scratch.home_cwd()),
    });
    client.wait_layout(10, "workspace w appears", |l| {
        l.squads.iter().any(|s| s.name == "w")
    });
    client.cmd(Command::SplitH);
    client.wait_layout(10, "first split lands", |l| {
        l.squads
            .iter()
            .any(|s| s.name == "w" && s.tabs.len() == 1 && s.panes == 2)
    });
    let tab = client
        .layout
        .as_ref()
        .and_then(|l| l.squads.iter().find(|s| s.name == "w"))
        .and_then(|s| s.tabs.first())
        .expect("workspace w has a tab")
        .id;
    client.cmd(Command::RenameTab {
        tab,
        name: "edit".into(),
    });
    client.wait_layout(10, "tab rename lands", |l| {
        l.squads
            .iter()
            .any(|s| s.name == "w" && s.tabs.len() == 1 && s.tabs[0].name == "edit")
    });
    client.cmd(Command::SplitH);
    client.wait_layout(10, "second split lands", |l| {
        l.squads
            .iter()
            .any(|s| s.name == "w" && s.tabs.len() == 1 && s.tabs[0].name == "edit" && s.panes == 3)
    });
}

fn assert_stored_three_pane_layout(scratch: &Scratch) {
    let path = scratch.0.join("iso-agents/squads.json");
    let raw = std::fs::read(&path)
        .unwrap_or_else(|e| panic!("read captured layout at {}: {e}", path.display()));
    let store: serde_json::Value = serde_json::from_slice(&raw).expect("captured layout is JSON");
    let squad = store["squads"]
        .as_array()
        .and_then(|squads| squads.iter().find(|s| s["name"] == "w"))
        .expect("captured store has squad w");
    let trees = squad["tab_trees"]
        .as_array()
        .expect("squad w has captured tab trees");
    assert_eq!(trees.len(), 1, "squad w must store exactly one tab");
    assert_eq!(trees[0]["tab_name"], "edit", "stored tab name");
    assert_eq!(
        trees[0]["slots"].as_array().map(Vec::len),
        Some(3),
        "stored tree must contain all three pane slots"
    );
}

fn assert_restored_three_pane_layout(client: &mut FakeClient) {
    client.wait_layout(15, "workspace w restores exactly", |l| {
        l.squads
            .iter()
            .any(|s| s.name == "w" && s.tabs.len() == 1 && s.tabs[0].name == "edit" && s.panes == 3)
    });
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
fn symptom_kill_server_restores_the_exact_layout() {
    let _g = PTY_GATE.lock().unwrap_or_else(|e| e.into_inner());
    let scratch = Scratch::new("kill-exact-layout");
    let server = spawn_server(&scratch.main_sock(), &[]);
    let mut client = attach_client(&scratch);
    drive_named_three_pane_layout(&mut client, &scratch);

    let _old = kill_server(&scratch, server, &mut client);
    assert_stored_three_pane_layout(&scratch);
    let _replacement = spawn_server(&scratch.main_sock(), &[]);
    let mut restored = attach_client(&scratch);
    assert_restored_three_pane_layout(&mut restored);
}

#[test]
fn symptom_worker_tab_position_and_pane_id_survive_tab_removal_and_restart() {
    let _g = PTY_GATE.lock().unwrap_or_else(|e| e.into_inner());
    let scratch = Scratch::new("worker-position");
    let server = spawn_server(&scratch.main_sock(), &[]);
    let mut client = attach_client(&scratch);
    drive_named_three_pane_layout(&mut client, &scratch);

    // The worker lane on the real user path: `pane run --worker` spawns the
    // pane through a keeper (the same out-of-process path every worker pane
    // takes) and records the squad member the store persists. An omitted
    // split mints the pane its own tab, so the worker tab is the SECOND one.
    let out = scratch
        .command()
        .args([
            "mux",
            "pane",
            "run",
            "--squad",
            "w",
            "--worker",
            "t-e2e-crew",
            "--",
            "sleep",
            "300",
        ])
        .output()
        .expect("pane run spawns the worker");
    assert!(
        out.status.success(),
        "worker spawn failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    client.wait_layout(10, "worker tab appears", |l| {
        l.squads.iter().any(|s| s.name == "w" && s.tabs.len() == 2)
    });
    let squad_tabs = |client: &FakeClient| -> Vec<(u64, String)> {
        client
            .layout
            .as_ref()
            .unwrap()
            .squads
            .iter()
            .find(|s| s.name == "w")
            .unwrap()
            .tabs
            .iter()
            .map(|t| (t.id, t.name.clone()))
            .collect()
    };
    let crew = squad_tabs(&client)[1].0;
    client.cmd(Command::RenameTab {
        tab: crew,
        name: "crew".into(),
    });

    // The persisted shape must hold the worker member with its birth pane id
    // before anything is removed: that id is the identity the final assertion
    // joins, read from the store rather than guessed.
    let read_store = |scratch: &Scratch| -> serde_json::Value {
        let path = scratch.0.join("iso-agents/squads.json");
        let raw = std::fs::read(&path).unwrap_or_default();
        serde_json::from_slice(&raw).unwrap_or(serde_json::Value::Null)
    };
    let squad_of = |doc: &serde_json::Value| -> serde_json::Value {
        doc["squads"]
            .as_array()
            .and_then(|ss| ss.iter().find(|s| s["name"] == "w"))
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    };
    let worker_member = |squad: &serde_json::Value| -> Option<serde_json::Value> {
        squad["members"]
            .as_array()
            .and_then(|ms| ms.iter().find(|m| m["worker"] == "t-e2e-crew"))
            .cloned()
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let pre_kill_pane_id = loop {
        let squad = squad_of(&read_store(&scratch));
        let member = worker_member(&squad);
        let trees = squad["tab_trees"].as_array().map(|t| t.len());
        let named = squad["tab_trees"][1]["tab_name"] == "crew";
        if let (Some(m), Some(2)) = (&member, trees) {
            if named && m["pane_id"].is_u64() {
                break m["pane_id"].as_u64().unwrap();
            }
        }
        assert!(
            Instant::now() < deadline,
            "store never captured the worker member with its birth pane id; last squad: {squad}"
        );
        std::thread::sleep(Duration::from_millis(100));
    };

    // Remove the FIRST tab, so the worker's captured index shifts to 0, and
    // wait for the capture to record the post-removal order.
    let first = squad_tabs(&client)[0].0;
    client.cmd(Command::SelectTab(first));
    client.cmd(Command::CloseTab);
    client.wait_layout(10, "first tab closes", |l| {
        l.squads.iter().any(|s| s.name == "w" && s.tabs.len() == 1)
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let squad = squad_of(&read_store(&scratch));
        let trees = squad["tab_trees"].as_array().map(|t| t.len());
        if trees == Some(1) && squad["tab_trees"][0]["tab_name"] == "crew" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "store never captured the post-removal tab order; last squad: {squad}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    let _old = kill_server(&scratch, server, &mut client);
    let _replacement = spawn_server(&scratch.main_sock(), &[]);
    let mut restored = attach_client(&scratch);

    // Positive markers, never absences: the worker tab comes back NAMED at
    // POSITION 0 holding its pane - not appended after a rebuilt shell tab,
    // not shell-substituted.
    restored.wait_layout(20, "worker tab restores at its captured position", |l| {
        l.squads
            .iter()
            .any(|s| s.name == "w" && s.tabs.len() == 1 && s.tabs[0].name == "crew" && s.panes == 1)
    });

    // The pane id is the identity that outlives the server: readopt
    // re-adopts the surviving keeper child at its birth id, so the live
    // listing must name the worker at exactly the id the pre-kill store
    // recorded. A shell substitute carries no worker name, so this read
    // fails loudly when the seating regressed.
    let deadline = Instant::now() + Duration::from_secs(10);
    let row = loop {
        let ls = scratch
            .command()
            .args(["mux", "pane", "ls"])
            .output()
            .expect("pane ls runs");
        let out = String::from_utf8_lossy(&ls.stdout).into_owned();
        if let Some(row) = out.lines().find(|l| l.contains("name=t-e2e-crew")) {
            break row.to_string();
        }
        assert!(
            Instant::now() < deadline,
            "pane ls never names the worker after the restart; output: {out}"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(
        row.split_whitespace().next() == Some(pre_kill_pane_id.to_string()).as_deref(),
        "the worker's pane ls row must lead with its surviving pane id {pre_kill_pane_id}: {row}"
    );
}

#[test]
fn symptom_kill_server_captures_without_a_dirty_flag() {
    let _g = PTY_GATE.lock().unwrap_or_else(|e| e.into_inner());
    let scratch = Scratch::new("kill-clean-layout");
    let server = spawn_server(&scratch.main_sock(), &[]);
    let mut client = attach_client(&scratch);
    drive_named_three_pane_layout(&mut client, &scratch);

    client.pump(Duration::from_secs(3));
    let tab = client
        .layout
        .as_ref()
        .and_then(|l| l.squads.iter().find(|s| s.name == "w"))
        .and_then(|s| s.tabs.first())
        .expect("workspace w has a tab")
        .id;
    client.cmd(Command::RenameTab {
        tab,
        name: "edit".into(),
    });
    client.pump(Duration::from_millis(500));
    assert_stored_three_pane_layout(&scratch);
    let store_path = scratch.0.join("iso-agents/squads.json");
    let before = std::fs::metadata(&store_path)
        .and_then(|m| m.modified())
        .expect("captured layout has an mtime before kill");
    std::thread::sleep(Duration::from_millis(1100));

    let _old = kill_server(&scratch, server, &mut client);
    let after = std::fs::metadata(&store_path)
        .and_then(|m| m.modified())
        .expect("captured layout has an mtime after kill");
    assert!(
        after > before,
        "clean topology must still be rewritten at teardown: before={before:?} after={after:?}"
    );
    assert_stored_three_pane_layout(&scratch);
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

/// The dedicated thread pane is never a persisted member, so restart must not
/// rebuild it. A pane binds a session to geometry, a thread binds a session
/// to a row, and a rebuilt thread pane would re-bind a thread to a rectangle
/// across a restart - the one property the substrate exists to avoid.
#[test]
fn symptom_restore_rebuilds_no_thread_pane() {
    let _g = PTY_GATE.lock().unwrap_or_else(|e| e.into_inner());
    let scratch = Scratch::new("thread-restore");
    // A `claude` stub on PATH that marks every spawn (`boot`) and every
    // `claude attach` spawn (`attach`). The pre-restart reach MUST produce
    // exactly one of each (the thread pane is an attach pane); restore must
    // produce NEITHER. The post-restart probe is the positive control: an
    // absent `boot` after restart means "not spawned", never "instrument
    // broken".
    let bin = scratch.0.join("stubbin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(
        bin.join("claude"),
        "#!/bin/sh\necho boot >> \"$STUB_MARKER\"\nif [ \"$1\" = attach ]; then echo attach >> \"$STUB_MARKER\"; fi\nsleep 60\n",
    )
    .unwrap();
    std::fs::set_permissions(bin.join("claude"), std::fs::Permissions::from_mode(0o755)).unwrap();
    // The reach resolves through the canonical re-entry plan, so the server
    // shells an `fno-agents` binary. None sits beside this TEST binary, so the
    // resolver falls to PATH: this stub answers with the bare resolved plan
    // this row's shape produces (claude, no route, no account), and the real
    // spawn still goes through the claude stub above.
    std::fs::write(
        bin.join("fno-agents"),
        concat!(
            "#!/bin/sh\n",
            "echo '{\"resolved\":true,\"argv\":[\"claude\",\"attach\",\"deadbee2\"],",
            "\"env\":{},\"claude_config_dir\":null}'\n",
        ),
    )
    .unwrap();
    std::fs::set_permissions(
        bin.join("fno-agents"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let marker = scratch.0.join("marker");
    let _ = std::fs::remove_file(&marker);
    let path_with_stub = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    // A LIVE paneless claude row: our own pid with no recorded start time, so
    // the liveness probe falls back to pid existence and the row reads as a
    // live daemon-hosted thread carrying an attach id.
    let agents_home = scratch.0.join("iso-agents");
    std::fs::create_dir_all(&agents_home).unwrap();
    std::fs::write(
        agents_home.join("registry.json"),
        format!(
            r#"{{"agents":[{{"name":"threader","cwd":"{}","status":"working","harness":"claude","short_id":"deadbee2","pid":{}}}]}}"#,
            scratch.home_cwd(),
            std::process::id()
        ),
    )
    .unwrap();

    let server = spawn_server(
        &scratch.main_sock(),
        &[
            ("PATH", path_with_stub.as_str()),
            ("STUB_MARKER", marker.to_str().unwrap()),
            // Pin the re-entry resolver to the STUB explicitly. This test's
            // design answers the reach with the canned plan above; it used to
            // get that by relying on no real `fno-agents` being discoverable
            // from the test binary - but the server's resolver lookup now
            // probes the dev-tree target dirs, so a freshly built sibling
            // (CI builds both crates) would win the PATH race and refuse on
            // this fixture's minimal row. The env var is the resolver's own
            // first-priority pin.
            ("FNO_AGENTS_BIN", bin.join("fno-agents").to_str().unwrap()),
        ],
    );
    let mut c = attach_client(&scratch);
    c.wait_layout(10, "the row surfaces", |l| {
        l.agents
            .iter()
            .any(|a| a.attach_id.as_deref() == Some("deadbee2"))
    });
    let live_panes = |c: &FakeClient| {
        c.layout
            .as_ref()
            .map(|l| l.squads.iter().map(|s| s.panes).sum::<usize>())
            .unwrap_or(0)
    };
    let before = live_panes(&c);

    // The reach: the exact command a TUI reach sends. One pane appears, and
    // it runs the stub's attach argv exactly once.
    c.cmd(Command::AttachAgent {
        id: "deadbee2".into(),
        placement: PanePlacement {
            thread_pane: true,
            ..Default::default()
        },
    });
    c.pump(Duration::from_secs(2));
    assert_eq!(
        live_panes(&c),
        before + 1,
        "the reach opened exactly one pane (notices so far: {:?})",
        c.notices
    );
    let text = std::fs::read_to_string(&marker).unwrap_or_default();
    assert_eq!(
        text.matches("boot").count(),
        1,
        "the reach spawned the attach argv once: {text:?}"
    );
    assert_eq!(
        text.matches("attach").count(),
        1,
        "the thread pane runs `claude attach`: {text:?}"
    );
    c.detach();

    // Restart: what "ending fno" does. The thread pane must not come back.
    // Pane-count equality is the wrong instrument here (restore's home-slot
    // rebuild for ANY captured topology is x-caef behavior, not thread
    // behavior); the thread contract is that NOTHING respawns for the thread:
    // no attach argv (the marker), and no slot for it in the captured trees
    // (asserted directly in the server unit tests). The replacement server
    // keeps the stub envs so the control probe below resolves the stub.
    let _old = server.terminate_and_wait();
    let _server2 = spawn_server(
        &scratch.main_sock(),
        &[
            ("PATH", path_with_stub.as_str()),
            ("STUB_MARKER", marker.to_str().unwrap()),
        ],
    );
    let mut r_client = attach_client(&scratch);
    r_client.wait_layout(10, "squads appear", |l| !l.squads.is_empty());
    r_client.pump(Duration::from_secs(3));
    let text = std::fs::read_to_string(&marker).unwrap_or_default();
    assert_eq!(
        text.matches("boot").count(),
        1,
        "restore respawned the thread pane's argv: {text:?}"
    );
    assert_eq!(
        text.matches("attach").count(),
        1,
        "restore rebuilt the thread pane as an attach: {text:?}"
    );

    // Positive control, AFTER the absence assertions: the stub is reachable
    // through the server's own spawn path.
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
    r_client.pump(Duration::from_secs(2));
    let text = std::fs::read_to_string(&marker).unwrap_or_default();
    assert_eq!(
        text.matches("boot").count(),
        2,
        "control failed: the stub never ran through the live spawn path: {text:?} (probe stdout: {}, probe stderr: {})",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    r_client.detach();
}
