//! A keeper-hosted pane spawned by this crate's tests must die with the test
//! run (fourteen `fno-agents-worker --pane` processes with ppid 1 and
//! `--session test` ran for hours on fno-mux-test-* sockets). The worker's
//! watchdog already exists (FNO_TEST_OWNER_PID/BIRTH, the fno-agents crate's
//! PR 1992); what was missing was the plumbing - this crate's test harness
//! spawned servers and keepers without the owner env. Two layers here: the
//! file guard that keeps spawn sites honest, and the positive control
//! proving the env flows harness -> server -> keeper.

mod common;
use common::{worker_bin, Scratch};

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Every test-source file that can spawn an `fno-agents-worker` keeper must
/// route the spawn through the owner-wiring harness (`Scratch` commands,
/// `spawn_server`, `self_owner_env`, or `spawn_keeper_for_test`). A raw
/// `Command::new` at a keeper path is exactly the orphan factory this node
/// closed, so a new file that names the worker without the wiring fails here.
#[test]
fn every_keeper_spawning_test_file_routes_through_the_owner_wiring() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // Test trees only. Production sources are never scanned: a production
    // keeper MUST outlive its spawner - that is the product contract.
    let mut sources = Vec::new();
    collect_rs(&manifest.join("tests"), &mut sources);
    sources.push(manifest.join("src/server_tests.rs"));
    collect_rs(&manifest.join("src/server/tests"), &mut sources);

    let exempt = ["product_boundary.rs"]; // asserts the missing-worker refusal; no keeper can spawn
    let wiring = [
        "self_owner_env",
        "spawn_keeper_for_test",
        "Scratch",
        "spawn_server",
    ];
    let mut matched = Vec::new();
    for path in &sources {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if exempt.contains(&name.as_str()) {
            continue;
        }
        let source = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let spawns_keepers = source.contains("fno-agents-worker")
            || source.contains("FNO_AGENTS_WORKER_BIN")
            || source.contains("spawn_keeper_for_test");
        if !spawns_keepers {
            continue;
        }
        matched.push(name.clone());
        assert!(
            wiring.iter().any(|w| source.contains(w)),
            "{} names the keeper worker without the test-owner wiring \
             (Scratch / spawn_server / self_owner_env / spawn_keeper_for_test): {}",
            name,
            path.display()
        );
    }
    assert!(
        matched.iter().any(|n| n == "agent_edge_e2e.rs"),
        "positive control: agent_edge_e2e.rs must be scanned"
    );
    assert!(
        matched.iter().any(|n| n == "mod.rs"),
        "positive control: tests/common/mod.rs must be scanned"
    );
    assert!(
        matched.iter().any(|n| n == "keeper_adopt_tests.rs"),
        "positive control: src/server/tests/keeper_adopt_tests.rs must be scanned"
    );
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_rs(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// The plumbing control, end to end: a server spawned by the harness, a
/// keeper pane spawned through it, and a SIGKILLed owner - after which the
/// keeper must die. The surrogate `sleep` stands in for THIS test binary
/// (killing ourselves is not an option); the harness `envs` override pins
/// the owner to it, which is also the proof the override path works.
#[test]
fn keeper_pane_dies_when_its_surrogate_test_owner_is_killed() {
    let scratch = Scratch::new("owner_reap");
    let dir = scratch.0.to_str().unwrap().to_string();

    let mut owner = Command::new("/bin/sleep")
        .arg("300")
        .spawn()
        .expect("surrogate owner spawns");
    let owner_pid = owner.id();
    let owner_birth = common::test_owner::process_start_time(owner_pid)
        .expect("a just-spawned process must have a readable birth time");

    let sock = scratch.main_sock();
    let _server = common::spawn_server(
        &sock,
        &[
            (
                "FNO_AGENTS_WORKER_BIN",
                worker_bin().to_string_lossy().as_ref(),
            ),
            ("FNO_TEST_OWNER_PID", &owner_pid.to_string()),
            ("FNO_TEST_OWNER_BIRTH", &owner_birth.to_string()),
        ],
    );
    let up = Instant::now() + Duration::from_secs(10);
    while !sock.exists() {
        assert!(Instant::now() < up, "server socket never appeared");
        std::thread::sleep(Duration::from_millis(25));
    }

    let run = scratch
        .command()
        .args([
            "mux",
            "pane",
            "run",
            "--worker",
            "owner-reap-worker",
            "--cwd",
            &dir,
            "--",
            "/bin/sh",
            "-c",
            "sleep 300",
        ])
        .env("FNO_AGENTS_WORKER_BIN", worker_bin())
        .output()
        .expect("fno binary runs");
    assert!(
        run.status.success(),
        "pane run stderr: {:?}",
        String::from_utf8_lossy(&run.stderr)
    );
    let _pane_id: u64 = String::from_utf8_lossy(&run.stdout)
        .trim()
        .parse()
        .expect("machine-readable pane id");

    // The keeper binds `{session}-{pane_key}.sock` under the mux dir's
    // panes/; this fresh session holds exactly one. Wait for the BIND before
    // pulling the owner out from under it, so a slow-starting keeper is never
    // mistaken for a reaped one.
    let panes = scratch.0.join("panes");
    let deadline = Instant::now() + Duration::from_secs(10);
    let keeper_sock = loop {
        let found: Vec<PathBuf> = std::fs::read_dir(&panes)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|x| x == "sock"))
                    .collect()
            })
            .unwrap_or_default();
        if let [only] = found.as_slice() {
            break only.clone();
        }
        assert!(
            Instant::now() < deadline,
            "keeper socket never appeared under {}: found {:?}",
            panes.display(),
            found
        );
        std::thread::sleep(Duration::from_millis(50));
    };

    // SIGKILL for real and reap - the positive control this test proves
    // against: the owner is CONFIRMED gone, not merely "should be by now".
    owner.kill().expect("owner SIGKILLs");
    owner.wait().expect("owner reaps");

    // The keeper unlinks its socket when the owner watchdog fires. A pid
    // probe is NOT the marker here: the server holds the keeper's Child
    // handle and never reaps it, so the dead keeper lingers as a zombie and
    // `kill(pid, 0)` reads a zombie as alive. 8s gives a loaded CI runner
    // headroom over the watchdog's 250ms poll interval.
    let deadline = Instant::now() + Duration::from_secs(8);
    while keeper_sock.exists() {
        assert!(
            Instant::now() < deadline,
            "keeper socket {} outlived its test owner",
            keeper_sock.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        std::os::unix::net::UnixStream::connect(&keeper_sock).is_err(),
        "a reaped keeper's socket must refuse connections"
    );
}
