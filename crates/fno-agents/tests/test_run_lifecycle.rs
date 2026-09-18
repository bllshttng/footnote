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

fn build_admit(root: &std::path::Path, cargo_pid: u32, worktree: &std::path::Path) -> Command {
    let mut cmd = Command::new(bin());
    cmd.args(["test-run", "build-admit", "--cargo-pid"])
        .arg(cargo_pid.to_string())
        .arg("--worktree")
        .arg(worktree)
        .env("FNO_CLAIMS_ROOT", root)
        .env("TMPDIR", root);
    cmd
}

fn build_holder(root: &std::path::Path) -> Option<String> {
    fno_agents::claims::status("build:cargo", Some(root))
        .1
        .map(|rec| rec.holder)
}

/// A second cargo waits on the first cargo's `build:cargo` claim, names the
/// holder while it waits, and is admitted once that cargo process exits.
#[test]
fn a_second_cargo_waits_until_the_building_cargo_exits() {
    let root = std::fs::canonicalize(tmp_claims_root("build-admit")).unwrap();
    let (tree_a, tree_b) = (root.join("a"), root.join("b"));
    std::fs::create_dir_all(&tree_a).unwrap();
    std::fs::create_dir_all(&tree_b).unwrap();
    let mut cargo_a = Command::new("sleep").arg("60").spawn().unwrap();
    let mut cargo_b = Command::new("sleep").arg("60").spawn().unwrap();

    let first = build_admit(&root, cargo_a.id(), &tree_a).status().unwrap();
    assert!(first.success(), "the first cargo must be admitted at once");
    let holder = build_holder(&root).expect("the first cargo holds build:cargo");
    assert!(holder.starts_with("cargo:"), "{holder}");

    let mut waiter = build_admit(&root, cargo_b.id(), &tree_b)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        waiter.try_wait().unwrap().is_none(),
        "the second cargo must hold while the first builds"
    );

    let _ = cargo_a.kill();
    let _ = cargo_a.wait();
    let released = Instant::now();
    let status = waiter.wait().unwrap();
    assert!(
        status.success(),
        "the waiter must be admitted, got {status}"
    );
    assert!(
        released.elapsed() < Duration::from_secs(2),
        "admission must follow the holder's exit within 2s, took {:?}",
        released.elapsed()
    );
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut waiter.stderr.take().unwrap(), &mut stderr).unwrap();
    assert!(
        stderr.contains("cargo admission: holding") && stderr.contains(&holder),
        "stderr must name the holder: {stderr}"
    );
    assert_ne!(build_holder(&root), Some(holder), "the waiter now holds");
    let _ = cargo_b.kill();
    let _ = cargo_b.wait();
    let _ = std::fs::remove_dir_all(&root);
}

/// A cargo started under the holding cargo (a test that runs cargo) is
/// admitted at once and leaves the claim with its ancestor.
#[test]
fn a_cargo_under_the_holding_cargo_is_admitted_at_once() {
    let root = std::fs::canonicalize(tmp_claims_root("build-nested")).unwrap();
    let first = build_admit(&root, std::process::id(), &root.join("outer"))
        .status()
        .unwrap();
    assert!(first.success());
    let holder = build_holder(&root).expect("this test process holds build:cargo");

    let mut nested_cargo = Command::new("sleep").arg("60").spawn().unwrap();
    let start = Instant::now();
    let nested = build_admit(&root, nested_cargo.id(), &root.join("inner"))
        .status()
        .unwrap();
    assert!(nested.success());
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(build_holder(&root), Some(holder));
    let _ = nested_cargo.kill();
    let _ = nested_cargo.wait();
    let _ = std::fs::remove_dir_all(&root);
}
