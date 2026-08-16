//! G1 script-API end-to-end: the `fno mux pane` verbs drive a live session
//! with no attached TUI client - the agents-spawn-agents smoke test. Every
//! test is hermetic (its own `FNO_MUX_DIR` tempdir + session) and drives the
//! real `fno` binary as a subprocess (the CLI surface), so it exercises
//! proto v4 + the server control loop + the CLI end to end.

mod common;
use common::Scratch;

use std::io::Read;
use std::os::unix::net::UnixListener;
use std::process::Output;
use std::time::{Duration, Instant};

/// Run `fno mux pane <args...>` against `scratch`'s session, headless.
fn pane(scratch: &Scratch, args: &[&str]) -> Output {
    scratch
        .command()
        .args(["mux", "pane"])
        .args(args)
        .env("SHELL", "/bin/sh")
        .output()
        .expect("fno binary runs")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Shut the session's server down (best effort) so a detached server never
/// outlives the test.
fn kill_server(scratch: &Scratch) {
    let _ = scratch.command().args(["mux", "kill-server"]).output();
}

fn write_registry(scratch: &Scratch, rows: &str) {
    let home = scratch.0.join("iso-agents");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("registry.json"),
        format!(r#"{{"schema_version":11,"agents":[{rows}]}}"#),
    )
    .unwrap();
}

/// Poll `pane ls --json` until it reports the empty listing (the session has
/// ended and its server is gone). Bounded so a stuck server fails loudly.
fn wait_ls_empty(scratch: &Scratch, secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let out = pane(scratch, &["ls", "--json"]);
        if out.status.success() && stdout(&out) == "[]" {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "pane ls never went empty within {secs}s; last exit={:?} stdout={:?} stderr={:?}",
                out.status.code(),
                stdout(&out),
                String::from_utf8_lossy(&out.stderr),
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn script_api_full_lifecycle_run_wait_read_kill_ls() {
    // AC 4.4: a script-only session's whole life via the CLI - run a pane that
    // echoes a marker, wait for it to settle, read it back, kill it, and see
    // the listing go empty when the last pane's exit ends the session.
    let scratch = Scratch::new("script_lifecycle");
    let dir = scratch.0.to_str().unwrap();

    let run = pane(
        &scratch,
        &[
            "run",
            "--cwd",
            dir,
            "--",
            "/bin/sh",
            "-c",
            "echo SCRIPT-MARKER-42; sleep 30",
        ],
    );
    assert!(
        run.status.success(),
        "run stderr: {:?}",
        String::from_utf8_lossy(&run.stderr)
    );
    let id = stdout(&run);
    assert!(
        id.parse::<u64>().is_ok(),
        "run must print exactly the pane id, got {id:?}"
    );

    // Settle: the echo prints, then the pane goes quiet.
    let wait = pane(
        &scratch,
        &["wait", &id, "--quiet-ms", "300", "--timeout", "10"],
    );
    assert_eq!(wait.status.code(), Some(0), "quiet settle is exit 0");
    assert_eq!(stdout(&wait), "quiet");

    // Read sees the marker on the visible grid.
    let read = pane(&scratch, &["read", &id]);
    assert!(read.status.success());
    assert!(
        stdout(&read).contains("SCRIPT-MARKER-42"),
        "read must see the marker, got {:?}",
        stdout(&read)
    );

    // This pane emitted no OSC 133 markers, so `--block last` degrades to ONE
    // implicit whole-output block, flagged (v6, AC2-ERR).
    let block = pane(&scratch, &["read", &id, "--block", "last", "--json"]);
    assert!(block.status.success());
    let bj = stdout(&block);
    assert!(
        bj.contains("SCRIPT-MARKER-42"),
        "implicit block text: {bj:?}"
    );
    assert!(bj.contains("\"implicit\":true"), "flagged implicit: {bj:?}");

    // Kill the only pane: the session ends and ls goes empty.
    let kill = pane(&scratch, &["kill", &id]);
    assert!(
        kill.status.success(),
        "kill stderr: {:?}",
        String::from_utf8_lossy(&kill.stderr)
    );
    wait_ls_empty(&scratch, 10);
}

#[test]
fn script_api_block_read_captures_span_exit_and_unavailable() {
    // v6 US1+US2 end to end: a pane emitting OSC 133 C/D markers captures one
    // command block; `read --block last --json` returns its output span with
    // seq/exit/complete, and a nonexistent seq is BLOCK_UNAVAILABLE (exit 14).
    let scratch = Scratch::new("script_block_read");
    let dir = scratch.0.to_str().unwrap();

    // printf emits: C (output start), the output line, D;7 (done, exit 7).
    let run = pane(
        &scratch,
        &[
            "run",
            "--cwd",
            dir,
            "--",
            "/bin/sh",
            "-c",
            r"printf '\033]133;C\ahello-block\n\033]133;D;7\a'; sleep 30",
        ],
    );
    assert!(
        run.status.success(),
        "run stderr: {:?}",
        String::from_utf8_lossy(&run.stderr)
    );
    let id = stdout(&run);

    // Let the markers settle through the server before reading.
    let wait = pane(
        &scratch,
        &["wait", &id, "--quiet-ms", "300", "--timeout", "10"],
    );
    assert_eq!(wait.status.code(), Some(0), "quiet settle is exit 0");

    let block = pane(&scratch, &["read", &id, "--block", "last", "--json"]);
    assert!(block.status.success(), "block read must succeed");
    let bj = stdout(&block);
    assert!(bj.contains("hello-block"), "block output span: {bj:?}");
    assert!(bj.contains("\"seq\":0"), "first block is seq 0: {bj:?}");
    assert!(bj.contains("\"exit\":7"), "exit recorded: {bj:?}");
    assert!(
        bj.contains("\"complete\":true"),
        "block is complete: {bj:?}"
    );
    assert!(bj.contains("\"implicit\":false"), "not implicit: {bj:?}");

    // A block that does not exist is BLOCK_UNAVAILABLE, tellable by exit code.
    let miss = pane(&scratch, &["read", &id, "--block", "99"]);
    assert_eq!(
        miss.status.code(),
        Some(14),
        "nonexistent block -> EXIT_BLOCK_UNAVAILABLE; stderr={:?}",
        String::from_utf8_lossy(&miss.stderr)
    );

    let _ = pane(&scratch, &["kill", &id]);
    wait_ls_empty(&scratch, 10);
}

#[test]
fn script_api_wait_command_done_returns_on_d_marker() {
    // v6 US3: `wait --command-done` resolves on the OSC 133 D marker with the
    // dedicated CommandDone exit code. The pane delays its markers so the wait
    // subscribes (baselining before any D) and then observes the D fire.
    let scratch = Scratch::new("script_command_done");
    let dir = scratch.0.to_str().unwrap();

    let run = pane(
        &scratch,
        &[
            "run",
            "--cwd",
            dir,
            "--",
            "/bin/sh",
            "-c",
            r"sleep 3; printf '\033]133;C\adone-out\n\033]133;D;0\a'; sleep 30",
        ],
    );
    assert!(run.status.success());
    let id = stdout(&run);

    // Issued immediately (well before the 3s delay), so the D fires after the
    // watcher subscribes -> CommandDone (exit 13), not a timeout.
    let wait = pane(
        &scratch,
        &["wait", &id, "--command-done", "--timeout", "15"],
    );
    assert_eq!(
        wait.status.code(),
        Some(13),
        "D marker -> EXIT_WAIT_COMMAND_DONE; stdout={:?} stderr={:?}",
        stdout(&wait),
        String::from_utf8_lossy(&wait.stderr)
    );
    assert_eq!(stdout(&wait), "command-done");

    let _ = pane(&scratch, &["kill", &id]);
    wait_ls_empty(&scratch, 10);
}

#[test]
fn script_api_wait_command_done_markerless_times_out_flagged() {
    // v6 AC3-FR: a markerless pane can never emit D, so --command-done resolves
    // by timeout (bounded, never infinite) and the CLI flags the degradation.
    let scratch = Scratch::new("script_command_done_markerless");
    let dir = scratch.0.to_str().unwrap();

    let run = pane(
        &scratch,
        &["run", "--cwd", dir, "--", "/bin/sh", "-c", "sleep 30"],
    );
    assert!(run.status.success());
    let id = stdout(&run);

    let wait = pane(
        &scratch,
        &["wait", &id, "--command-done", "--timeout", "2", "--json"],
    );
    assert_eq!(
        wait.status.code(),
        Some(11),
        "markerless --command-done -> EXIT_WAIT_TIMEOUT; stdout={:?}",
        stdout(&wait)
    );
    assert!(
        stdout(&wait).contains("\"degraded\""),
        "the degradation must be flagged in --json, got {:?}",
        stdout(&wait)
    );

    let _ = pane(&scratch, &["kill", &id]);
    wait_ls_empty(&scratch, 10);
}

#[test]
fn script_api_dead_pane_verbs_fail_closed() {
    // AC4-ERR: read/send/wait/kill on a dead pane id fail closed (nonzero),
    // never hang. Start a real server with one live pane, then target a bogus
    // id. `pane wait` on a dead id must return promptly, not sit out a timeout.
    let scratch = Scratch::new("script_dead_pane");
    let dir = scratch.0.to_str().unwrap();
    let run = pane(
        &scratch,
        &["run", "--cwd", dir, "--", "/bin/sh", "-c", "sleep 30"],
    );
    assert!(run.status.success());

    for verb in [
        vec!["read", "9999"],
        vec!["send", "9999", "--text", "x"],
        vec!["wait", "9999", "--timeout", "5"],
        vec!["kill", "9999"],
    ] {
        let started = Instant::now();
        let out = pane(&scratch, &verb);
        assert_eq!(
            out.status.code(),
            Some(1),
            "{verb:?} on a dead pane must exit 1; stderr={:?}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{verb:?} on a dead pane must fail fast, took {:?}",
            started.elapsed()
        );
    }
    kill_server(&scratch);
}

#[test]
fn script_api_version_skew_refused_loudly() {
    // AC4-FR: a v4 control verb against a server that cannot parse it (a v3
    // build) is refused loudly, naming this client's proto. A real v3 server
    // reads the Control frame it cannot deserialize and closes; the stub here
    // does exactly that (bind, accept, read, close - no reply).
    let scratch = Scratch::new("script_version_skew");
    let sock = scratch.main_sock();
    let listener = UnixListener::bind(&sock).expect("bind stub server");
    let stub = std::thread::spawn(move || {
        if let Ok((mut s, _)) = listener.accept() {
            let mut buf = [0u8; 64];
            let _ = s.read(&mut buf); // consume (part of) the Control frame, then close
        }
    });

    let out = pane(&scratch, &["ls"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "a version-skewed control connection must exit 1"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("proto"),
        "the refusal must name the protocol version, got {stderr:?}"
    );
    stub.join().ok();
}

#[test]
fn script_api_concurrent_runs_land_three_panes_in_one_squad() {
    // AC 4.4 + the impatient-user finding: three concurrent runs into ONE cwd
    // become three panes in one squad - no false dedup at the mux layer (dedup
    // lives in the spawn front half, not here) - and the concurrent
    // self-spawn race converges on one server (AC1-EDGE).
    let scratch = std::sync::Arc::new(Scratch::new("script_concurrent"));
    let dir = scratch.0.to_str().unwrap().to_string();

    let handles: Vec<_> = (0..3)
        .map(|_| {
            let scratch = std::sync::Arc::clone(&scratch);
            let cwd = dir.clone();
            std::thread::spawn(move || {
                scratch
                    .command()
                    .args([
                        "mux", "pane", "run", "--cwd", &cwd, "--", "/bin/sh", "-c", "sleep 30",
                    ])
                    .env("SHELL", "/bin/sh")
                    .output()
                    .expect("fno runs")
                    .status
                    .success()
            })
        })
        .collect();
    for h in handles {
        assert!(h.join().unwrap(), "each concurrent run must succeed");
    }

    let ls = pane(&scratch, &["ls"]);
    assert!(ls.status.success());
    let listing = stdout(&ls);
    let lines: Vec<&str> = listing.lines().collect();
    assert_eq!(
        lines.len(),
        3,
        "three runs -> three panes, got: {:?}",
        lines
    );
    let squads: std::collections::HashSet<&str> = lines
        .iter()
        .filter_map(|l| l.split_whitespace().find(|f| f.starts_with("squad=")))
        .collect();
    assert_eq!(
        squads.len(),
        1,
        "all three panes share one squad, got {squads:?}"
    );
    kill_server(&scratch);
}

#[test]
fn script_api_pane_run_self_spawns_into_nonexistent_mux_dir() {
    // Regression: pane run's self-spawn path opens the server log
    // inside the mux dir. On a fresh machine with no ~/.fno/mux the spawn must
    // still succeed, because connect_or_spawn now ensures the dir first. Every
    // other test missed this by pointing FNO_MUX_DIR at a pre-created tempdir,
    // so here we deliberately aim it at a dir that does NOT exist yet.
    // Remove the scratch dir Scratch::new just created so FNO_MUX_DIR points at
    // a path that does not exist - but keep it at the same depth as every other
    // test (nesting a deeper subdir would blow the AF_UNIX sun_path limit under
    // macOS's long temp dir, not the bug under test).
    let scratch = Scratch::new("ens");
    let mux_dir = scratch.0.clone();
    std::fs::remove_dir_all(&mux_dir).unwrap();
    assert!(
        !mux_dir.exists(),
        "precondition: mux dir must not exist yet"
    );

    let run = scratch
        .command()
        .args(["mux", "pane", "run", "--", "/bin/sh", "-c", "sleep 30"])
        .env("SHELL", "/bin/sh")
        .output()
        .expect("fno binary runs");
    assert!(
        run.status.success(),
        "self-spawn into a nonexistent mux dir must succeed; stderr: {:?}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(mux_dir.exists(), "connect_or_spawn must create the mux dir");
}

#[test]
fn defensive_reaper_sweeps_a_pane_when_its_exit_notification_is_lost() {
    let scratch = Scratch::new("reaper_tick");
    let run = scratch
        .command()
        .args(["mux", "pane", "run", "--", "/bin/sh", "-c", "sleep 0.2"])
        .env("FNO_E2E_DROP_PTY_EXIT", "1")
        .env("SHELL", "/bin/sh")
        .output()
        .expect("fno binary runs");
    assert!(
        run.status.success(),
        "pane run must start the isolated server: {:?}",
        String::from_utf8_lossy(&run.stderr)
    );

    let deadline = Instant::now() + Duration::from_secs(8);
    while scratch.main_sock().exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !scratch.main_sock().exists(),
        "the periodic reaper must remove the last dead pane and shut down the server"
    );
    let log = std::fs::read_to_string(scratch.0.join("main.log")).unwrap_or_default();
    assert!(
        log.contains("deliberately dropped exit") && log.contains("last dead pane reaped"),
        "the test must exercise the lost-exit timer path; log: {log}"
    );
}

#[test]
fn zero_viewer_identity_join_reads_fresh_registry_and_real_claim() {
    let scratch = Scratch::new("zero_viewer_identity");
    let dir = scratch.0.to_str().unwrap();
    let run = pane(
        &scratch,
        &["run", "--cwd", dir, "--", "/bin/sh", "-c", "sleep 30"],
    );
    assert!(run.status.success());
    let pane_id = stdout(&run);
    let full_id = "019fb024-2327-75f3-8b80-06e9d5ade05f";
    write_registry(
        &scratch,
        &format!(
            r#"{{"name":"requested-name","cwd":"{dir}","harness":"codex","harness_session_id":"{full_id}","status":"live","mux":{{"session":"main","pane_id":{pane_id}}}}}"#
        ),
    );

    let ls = pane(&scratch, &["ls", "--json"]);
    assert!(
        ls.status.success(),
        "pane ls stderr: {:?}",
        String::from_utf8_lossy(&ls.stderr)
    );
    let listing = stdout(&ls);
    assert!(
        listing.contains(&format!(r#""fno_id":"{full_id}""#)),
        "fresh fno_id: {listing}"
    );

    for handle in [full_id, "019fb024"] {
        let located = scratch
            .command()
            .args(["mux", "where", handle, "--json"])
            .output()
            .unwrap();
        assert!(
            located.status.success(),
            "where {handle} stderr: {:?}",
            String::from_utf8_lossy(&located.stderr)
        );
        let location = stdout(&located);
        assert!(location.contains(&format!(r#""fno_id":"{handle}""#)));
        assert!(location.contains(&format!(r#""panes":[{pane_id}]"#)));
    }

    let claim = scratch
        .command()
        .args([
            "claim",
            "acquire",
            "node:x-f0c2-verify",
            "--holder",
            full_id,
            "--ttl",
            "60s",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        claim.status.success(),
        "claim stderr: {:?}",
        String::from_utf8_lossy(&claim.stderr)
    );
    assert!(
        stdout(&claim).contains(full_id),
        "claim holder must be the peer identity"
    );

    let _ = pane(&scratch, &["kill", &pane_id]);
    wait_ls_empty(&scratch, 10);
}

#[test]
fn mux_where_cli_rejects_harness_only_ambiguous_prefix() {
    let scratch = Scratch::new("where_ambiguous_harness");
    write_registry(
        &scratch,
        r#"{"name":"a","cwd":"/a","harness":"codex","harness_session_id":"019fb024-one","status":"live","mux":{"session":"main","pane_id":1}},{"name":"b","cwd":"/b","harness":"codex","harness_session_id":"019fb024-two","status":"live","mux":{"session":"main","pane_id":2}}"#,
    );
    let out = scratch
        .command()
        .args(["mux", "where", "019fb024", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(16));
    assert!(String::from_utf8_lossy(&out.stderr).contains("ambiguous prefix"));
}

#[test]
fn server_publishes_and_removes_its_pid_sidecar() {
    // x-48a5: the server writes its pid beside its socket at bind so
    // kill-server can signal a wedged holder without an accepted connection,
    // and every exit path removes it with the socket. A real headless server,
    // watched across its whole life.
    let scratch = Scratch::new("pid_sidecar");
    let server = common::spawn_server(&scratch.main_sock(), &[]);

    let pid_path = scratch.0.join("main.pid");
    let deadline = Instant::now() + Duration::from_secs(5);
    // Content is "<pid>:<start_time>" (x-48a5) or bare "<pid>" on a platform
    // pid_start_time cannot supply one for; only the pid field matters here.
    let pid: i32 = loop {
        if let Some(p) = std::fs::read_to_string(&pid_path)
            .ok()
            .and_then(|t| t.trim().split(':').next()?.parse().ok())
        {
            break p;
        }
        assert!(
            Instant::now() < deadline,
            "pid sidecar never appeared at {}",
            pid_path.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        unsafe { libc::kill(pid, 0) } == 0,
        "sidecar pid {pid} must be live right after bind"
    );

    let out = scratch
        .command()
        .args(["mux", "kill-server", "--json"])
        .output()
        .expect("kill-server runs");
    assert!(
        out.status.success(),
        "kill-server stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(v["killed"], true, "healthy server dies gracefully");
    // Under load the graceful window can expire and the SIGTERM rung finishes
    // the same clean shutdown; both are clean deaths of a healthy server.
    assert!(
        v["path"] == "graceful" || v["path"] == "sigterm",
        "path names a clean-death rung: {}",
        v["path"]
    );

    for name in ["main.sock", "main.ver", "main.pid"] {
        let p = scratch.0.join(name);
        let deadline = Instant::now() + Duration::from_secs(5);
        while p.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!p.exists(), "{name} left behind after shutdown");
    }
    drop(server);
}

#[test]
fn sigterm_shutdown_kills_pane_children() {
    // x-48a5: the SIGTERM arm must tear panes down the way CoreMsg::Kill
    // does. The pane holds a child that ignores SIGHUP, so the closing pty
    // master cannot be what kills it - only the server's explicit teardown
    // can. Pre-fix, the child outlives the server.
    let scratch = Scratch::new("sigterm_panes");
    let dir = scratch.0.to_str().unwrap();
    let run = pane(
        &scratch,
        &[
            "run",
            "--cwd",
            dir,
            "--",
            "/bin/sh",
            "-c",
            "trap '' HUP; exec sleep 300",
        ],
    );
    assert!(
        run.status.success(),
        "run stderr: {:?}",
        String::from_utf8_lossy(&run.stderr)
    );

    // The pane's child pid, straight from the listing.
    let ls = stdout(&pane(&scratch, &["ls", "--json"]));
    let panes: Vec<serde_json::Value> = serde_json::from_str(&ls).unwrap();
    let child_pid = panes[0]["child_pid"]
        .as_u64()
        .expect("pane reports child_pid") as i32;
    assert!(
        unsafe { libc::kill(child_pid, 0) } == 0,
        "pane child {child_pid} live before SIGTERM"
    );

    let server_pid: i32 = std::fs::read_to_string(scratch.0.join("main.pid"))
        .expect("server pid sidecar")
        .trim()
        .split(':')
        .next()
        .expect("pid field")
        .parse()
        .unwrap();
    unsafe { libc::kill(server_pid, libc::SIGTERM) };

    let deadline = Instant::now() + Duration::from_secs(2);
    while unsafe { libc::kill(child_pid, 0) } == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        unsafe { libc::kill(child_pid, 0) } != 0,
        "pane child {child_pid} outlived the server's SIGTERM"
    );
    drop(scratch);
}
