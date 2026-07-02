//! G1 script-API end-to-end: the `fno mux pane` verbs drive a live session
//! with no attached TUI client - the agents-spawn-agents smoke test. Every
//! test is hermetic (its own `FNO_MUX_DIR` tempdir + session) and drives the
//! real `fno` binary as a subprocess (the CLI surface), so it exercises
//! proto v4 + the server control loop + the CLI end to end.

mod common;
use common::Scratch;

use std::io::Read;
use std::os::unix::net::UnixListener;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// Run `fno mux pane <args...>` against `scratch`'s session, headless.
fn pane(scratch: &Scratch, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fno"))
        .args(["mux", "pane"])
        .args(args)
        .env("FNO_MUX_DIR", &scratch.0)
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
    let _ = Command::new(env!("CARGO_BIN_EXE_fno"))
        .args(["mux", "kill-server"])
        .env("FNO_MUX_DIR", &scratch.0)
        .output();
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
    let scratch = Scratch::new("script_concurrent");
    let dir = scratch.0.to_str().unwrap().to_string();

    let handles: Vec<_> = (0..3)
        .map(|_| {
            let mux_dir = scratch.0.clone();
            let cwd = dir.clone();
            std::thread::spawn(move || {
                Command::new(env!("CARGO_BIN_EXE_fno"))
                    .args([
                        "mux", "pane", "run", "--cwd", &cwd, "--", "/bin/sh", "-c", "sleep 30",
                    ])
                    .env("FNO_MUX_DIR", &mux_dir)
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
