//! A test-owned keeper (pane or graph store) must die with the test run that
//! spawned it, never surviving on the production keeper's outlive-anything
//! contract (x-79bc: `fno-agents-worker --pane` processes with dead pids
//! embedded in their `--sock` path, orphaned because nothing ever bound their
//! lifetime to the test that started them). Real processes throughout: the
//! "owner" is an actual short-lived pid, killed for real, with the keeper's
//! own death confirmed by protocol (Identify refusal) and process liveness -
//! never a log line alone.

use std::io::Read;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn worker_bin() -> &'static str {
    env!("CARGO_BIN_EXE_fno-agents-worker")
}

fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 is the existence probe; no signal is delivered.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// A short scratch dir: the unix socket path built under it must stay under
/// `SUN_LEN` (~104 bytes on macOS), which a nanosecond timestamp component
/// blew past.
fn scratch_dir(tag: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("fno-tok-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct OwnerProcess(Child);

impl OwnerProcess {
    fn spawn() -> (Self, u32, u64) {
        let child = Command::new("sleep")
            .arg("300")
            .spawn()
            .expect("spawn owner process");
        let pid = child.id();
        let birth = fno_agents::daemon::process_start_time(pid)
            .expect("a just-spawned process must have a readable birth time");
        (Self(child), pid, birth)
    }

    /// Kill for real and reap - the positive control this whole test proves
    /// against: the owner is CONFIRMED gone, not merely "should be by now".
    fn kill_and_reap(mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        assert!(
            !alive(self.0.id()),
            "the owner itself must be dead before asserting on it"
        );
    }
}

struct KillGuard(u32);
impl Drop for KillGuard {
    fn drop(&mut self) {
        unsafe { libc::kill(self.0 as libc::pid_t, libc::SIGKILL) };
    }
}

#[test]
fn pane_keeper_dies_within_5s_of_its_test_owner() {
    let dir = scratch_dir("pane");
    let sock = dir.join("keeper.sock");
    let (owner, owner_pid, owner_birth) = OwnerProcess::spawn();

    let mut keeper = Command::new(worker_bin())
        .args([
            "--pane",
            "--sock",
            sock.to_str().unwrap(),
            "--session",
            "t",
            "--pane-key",
            "1",
            "--cwd",
            "/tmp",
            "--",
        ])
        .arg("sleep")
        .arg("300")
        .env("FNO_TEST_OWNER_PID", owner_pid.to_string())
        .env("FNO_TEST_OWNER_BIRTH", owner_birth.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pane keeper");
    let keeper_pid = keeper.id();
    let _guard = KillGuard(keeper_pid);

    // Wait for the keeper to come up before pulling its owner out from
    // under it, so a slow-starting keeper is never mistaken for a reaped one.
    let up_deadline = Instant::now() + Duration::from_secs(10);
    while UnixStream::connect(&sock).is_err() {
        assert!(
            Instant::now() < up_deadline,
            "pane keeper socket never appeared"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    owner.kill_and_reap();

    // `try_wait` both checks AND reaps: a raw `kill(pid, 0)` liveness probe
    // reads a zombie (already exited, not yet reaped by ITS parent - this
    // process) as "still alive", which is exactly backwards for a keeper
    // this test's own `Child` handle owns the reap of.
    // 8s gives a loaded CI runner headroom over the mechanism's real 250ms
    // poll interval; the assertion still fails if reaping is actually broken.
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        match keeper.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(e) => panic!("try_wait on pane keeper {keeper_pid} failed: {e}"),
        }
        assert!(
            Instant::now() < deadline,
            "pane keeper {keeper_pid} must die of its test owner's death"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!sock.exists(), "a reaped keeper must unlink its socket");
}

#[test]
fn graph_keeper_dies_within_5s_of_its_test_owner() {
    let dir = scratch_dir("graph");
    let sock = dir.join("store.sock");
    let graph = dir.join("graph.json");
    std::fs::write(&graph, "{\n  \"entries\": []\n}\n").unwrap();
    let (owner, owner_pid, owner_birth) = OwnerProcess::spawn();

    let mut keeper = Command::new(worker_bin())
        .args([
            "--store-keeper",
            "--sock",
            sock.to_str().unwrap(),
            "--graph",
            graph.to_str().unwrap(),
            "--session",
            "t",
        ])
        .env("FNO_TEST_OWNER_PID", owner_pid.to_string())
        .env("FNO_TEST_OWNER_BIRTH", owner_birth.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn graph keeper");
    let keeper_pid = keeper.id();
    let _guard = KillGuard(keeper_pid);

    let up_deadline = Instant::now() + Duration::from_secs(10);
    while UnixStream::connect(&sock).is_err() {
        assert!(
            Instant::now() < up_deadline,
            "graph keeper socket never appeared"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    owner.kill_and_reap();

    // `try_wait` both checks AND reaps (see the pane-keeper test's note on
    // why a raw `kill(pid, 0)` liveness probe reads a zombie as "alive").
    // 8s gives a loaded CI runner headroom over the mechanism's real 250ms
    // poll interval; the assertion still fails if reaping is actually broken.
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        match keeper.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(e) => panic!("try_wait on graph keeper {keeper_pid} failed: {e}"),
        }
        assert!(
            Instant::now() < deadline,
            "graph keeper {keeper_pid} must die within 5s of its test owner's death"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // A reaped keeper's socket must refuse new connections - the positive
    // marker; a lingering listener that still answers protocol frames is
    // not "basically reaped".
    assert!(
        UnixStream::connect(&sock)
            .and_then(|mut s| {
                let mut buf = [0u8; 1];
                s.read(&mut buf)
            })
            .is_err(),
        "a reaped graph keeper's socket must refuse connections"
    );
}

/// A PRODUCTION keeper (no test-owner env at all) must be unaffected by this
/// feature - Locked Decision 4's positive control, run here alongside the
/// new negative-lifetime tests so a regression in either direction is caught
/// in the same file.
#[test]
fn a_keeper_with_no_test_owner_env_ignores_an_unrelated_dead_pid() {
    let dir = scratch_dir("production");
    let sock = dir.join("keeper.sock");
    // A pid guaranteed dead and unrelated to this test.
    let (throwaway, throwaway_pid, throwaway_birth) = OwnerProcess::spawn();
    throwaway.kill_and_reap();
    let _ = (throwaway_pid, throwaway_birth); // never wired to the keeper below

    let keeper = Command::new(worker_bin())
        .args([
            "--pane",
            "--sock",
            sock.to_str().unwrap(),
            "--session",
            "t",
            "--pane-key",
            "1",
            "--cwd",
            "/tmp",
            "--",
        ])
        .arg("sleep")
        .arg("2")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pane keeper");
    let keeper_pid = keeper.id();
    let _guard = KillGuard(keeper_pid);

    let up_deadline = Instant::now() + Duration::from_secs(10);
    while UnixStream::connect(&sock).is_err() {
        assert!(
            Instant::now() < up_deadline,
            "pane keeper socket never appeared"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // No FNO_TEST_OWNER_* env was set, so the watcher thread never spawned:
    // the keeper must still be alive well past the 250ms poll interval this
    // feature introduces.
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        alive(keeper_pid),
        "a production keeper must never be reaped by this feature"
    );
}
