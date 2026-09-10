//! Real-process proof for `fno-agents test-run` (the native owner in
//! `src/test_run.rs`): a lingering group-mate left behind by a leader's
//! NORMAL exit gets reaped, a hung leader gets killed on timeout, and two
//! concurrent runs serialize under the shared `test:suite` claim. Every
//! assertion is a positive marker on a real pid (born from an actual short
//! command), never an absence read off a log line.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_fno-agents")
}

/// Liveness by the same primitive `test_run.rs` uses: a `kill(pid, 0)` probe.
fn pid_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::c_int, 0) == 0 }
}

fn tmp_claims_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "fno-test-run-lifecycle-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create claims root");
    dir
}

/// The x-b275 shape: a leader that backgrounds a child and exits immediately,
/// leaving that child alive in the SAME process group (no job control under
/// `sh -c`, so the background job never gets its own pgid). The old
/// `wait_or_kill_group` only killed on timeout/exception; this proves the
/// native owner kills it on a plain, successful, on-time exit too.
#[test]
fn normal_exit_still_reaps_a_backgrounded_group_mate() {
    let root = tmp_claims_root("normal-exit");
    let pid_file = root.join("leftover.pid");
    let status = Command::new(bin())
        .args(["test-run", "--timeout", "30", "--claims-root"])
        .arg(&root)
        .arg("--")
        .arg("/bin/sh")
        .arg("-c")
        .arg(format!(
            "sleep 20 & echo $! > {}; exit 0",
            pid_file.display()
        ))
        .status()
        .expect("run fno-agents test-run");
    assert!(
        status.success(),
        "leader's own exit must still read as success"
    );

    let leftover_pid: u32 = std::fs::read_to_string(&pid_file)
        .expect("leader must have written the backgrounded pid before exiting")
        .trim()
        .parse()
        .expect("pid file must contain a bare pid");

    // The leader itself exited immediately; the assertion is on the CHILD it
    // left behind, which the group-kill must have already reached by the
    // time `Command::status()` returned above (cleanup runs before this
    // process's own exit code is decided).
    assert!(
        !pid_alive(leftover_pid),
        "backgrounded sleep {leftover_pid} must be dead: a normal leader exit must still \
         terminate every group-mate it leaves running"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A leader that never exits on its own must be killed, with a distinct exit
/// code naming the timeout (124), and be gone shortly after.
#[test]
fn timeout_kills_the_hung_leader() {
    let root = tmp_claims_root("timeout");
    let start = Instant::now();
    let child = Command::new(bin())
        .args(["test-run", "--timeout", "1", "--claims-root"])
        .arg(&root)
        .arg("--")
        .arg("sleep")
        .arg("30")
        .spawn()
        .expect("spawn fno-agents test-run");
    let output = child.wait_with_output().expect("wait for test-run");
    let elapsed = start.elapsed();
    assert_eq!(
        output.status.code(),
        Some(124),
        "a hung leader must exit 124"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "the 1s timeout plus cleanup must not need anywhere near the leader's 30s sleep \
         (took {elapsed:?})"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Two runs sharing a claims root must serialize: the second spawns nothing
/// until the first releases. Discriminated by wall-clock total, not a log
/// line - a broken (parallel) admission finishes in ~1s, a working
/// (serialized) one takes near 2s.
#[test]
fn concurrent_runs_serialize_under_the_shared_claim() {
    let root = tmp_claims_root("admission");
    let start = Instant::now();

    let mut first = Command::new(bin())
        .args(["test-run", "--timeout", "30", "--claims-root"])
        .arg(&root)
        .arg("--")
        .arg("sleep")
        .arg("1")
        .spawn()
        .expect("spawn first test-run");
    // Give the first run a head start on the claim so the second reliably
    // observes it Live rather than racing the lockfile create.
    std::thread::sleep(Duration::from_millis(200));
    let mut second = Command::new(bin())
        .args(["test-run", "--timeout", "30", "--claims-root"])
        .arg(&root)
        .arg("--")
        .arg("sleep")
        .arg("1")
        .spawn()
        .expect("spawn second test-run");

    let first_status = first.wait().expect("wait first");
    let second_status = second.wait().expect("wait second");
    let elapsed = start.elapsed();

    assert!(first_status.success());
    assert!(second_status.success());
    assert!(
        elapsed >= Duration::from_millis(1800),
        "two 1s runs under one admission claim must serialize to ~2s total, took {elapsed:?} \
         (a parallel/broken claim would finish in ~1s)"
    );
    let _ = std::fs::remove_dir_all(&root);
}
