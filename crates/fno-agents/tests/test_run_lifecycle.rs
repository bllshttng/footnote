//! Real-process proof for `fno-agents test-run` (the native owner in
//! `src/test_run.rs`): a lingering group-mate left behind by a leader's
//! NORMAL exit gets reaped, a hung leader gets killed on timeout, and two
//! concurrent runs serialize under the shared `test:suite` claim. Every
//! assertion is a positive marker on a real pid (born from an actual short
//! command), never an absence read off a log line.

use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_fno-agents")
}

/// A `test-run` command on a scratch claims root with the owner-identity env
/// stripped. Under `fno doctor test rust` the outer owner puts its identity
/// in the env, so a spawned test-run reads itself as nested and never takes
/// the claim; a test that measures contention must strip it.
fn test_run(root: &std::path::Path) -> Command {
    let mut cmd = Command::new(bin());
    cmd.args(["test-run", "--claims-root"]).arg(root);
    cmd.env_remove("FNO_TEST_OWNER_PID");
    cmd.env_remove("FNO_TEST_OWNER_BIRTH");
    cmd
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

/// A leader that backgrounds a child and exits immediately, leaving that
/// child alive in the SAME process group (no job control under `sh -c`, so
/// the background job never gets its own pgid). The old `wait_or_kill_group`
/// only killed on timeout/exception; this proves the native owner kills it on
/// a plain, successful, on-time exit too.
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
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn fno-agents test-run");
    let output = child.wait_with_output().expect("wait for test-run");
    let elapsed = start.elapsed();
    assert_eq!(
        output.status.code(),
        Some(124),
        "a hung leader must exit 124"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("0s queued"),
        "an uncontended timeout names its zero-second wait: {stderr}"
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

    let mut first = test_run(&root)
        .args(["--timeout", "30"])
        .arg("--")
        .arg("sleep")
        .arg("1")
        .spawn()
        .expect("spawn first test-run");
    // Give the first run a head start on the claim so the second reliably
    // observes it Live rather than racing the lockfile create.
    std::thread::sleep(Duration::from_millis(200));
    let mut second = test_run(&root)
        .args(["--timeout", "30"])
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

/// AC1-HP: a waiter queued behind a live holder names that holder, its pid
/// and its liveness on the waiting line; `holder=-` never prints.
#[test]
fn a_queued_waiter_names_the_live_holder() {
    let root = tmp_claims_root("wait-names");
    let mut holder = test_run(&root)
        .args(["--timeout", "30"])
        .arg("--")
        .arg("sleep")
        .arg("6")
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn holder");
    std::thread::sleep(Duration::from_millis(300));
    let w1 = test_run(&root)
        .args(["--timeout", "30"])
        .arg("--")
        .arg("true")
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn waiter 1");
    let w2 = test_run(&root)
        .args(["--timeout", "30"])
        .arg("--")
        .arg("true")
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn waiter 2");

    let out1 = w1.wait_with_output().expect("wait waiter 1");
    let out2 = w2.wait_with_output().expect("wait waiter 2");
    assert!(
        holder.try_wait().unwrap().is_some(),
        "the holder must have exited"
    );
    let _ = holder.wait();
    assert!(out1.status.success() && out2.status.success());
    for (n, out) in [(1, &out1), (2, &out2)] {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let line = stderr
            .lines()
            .find(|l| l.contains("suite_waiting"))
            .unwrap_or_else(|| panic!("waiter {n} must print a waiting line: {stderr}"));
        assert!(line.contains("claim=test:suite"), "{line}");
        assert!(line.contains("holder_state=live"), "{line}");
        assert!(!line.contains("holder=-"), "{line}");
        let pid = line
            .split(" pid=")
            .nth(1)
            .unwrap_or_else(|| panic!("no pid field: {line}"))
            .split(' ')
            .next()
            .unwrap();
        assert!(
            !pid.is_empty() && pid.chars().all(|c| c.is_ascii_digit()),
            "pid must print as digits, got {line}"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// AC2-HP: a queued waiter repeats its waiting line at the notice interval,
/// so a 3s wait holds exactly one `suite_waiting` line, naming the holder.
#[test]
fn a_queued_waiter_prints_its_waiting_line_once() {
    let root = tmp_claims_root("wait-once");
    let mut holder = test_run(&root)
        .args(["--timeout", "30"])
        .arg("--")
        .arg("sleep")
        .arg("3")
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn holder");
    std::thread::sleep(Duration::from_millis(300));
    let out = test_run(&root)
        .args(["--timeout", "30"])
        .arg("--")
        .arg("true")
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("run waiter");
    assert!(
        holder.try_wait().unwrap().is_some(),
        "the holder must have exited"
    );
    let _ = holder.wait();
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    let waiting = stderr
        .lines()
        .filter(|l| l.contains("suite_waiting"))
        .count();
    assert_eq!(waiting, 1, "one throttled waiting line, got:\n{stderr}");
    assert!(stderr.contains("holder_state=live"), "{stderr}");
    let _ = std::fs::remove_dir_all(&root);
}

/// AC4-HP: SIGTERM to a queued waiter stops it within 2s with exit 143 and a
/// `suite_wait_interrupted` receipt naming the signal and the wait so far.
#[test]
fn a_queued_waiter_stops_on_sigterm() {
    let root = tmp_claims_root("wait-term");
    let mut holder = test_run(&root)
        .args(["--timeout", "20"])
        .arg("--")
        .arg("sleep")
        .arg("20")
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn holder");
    std::thread::sleep(Duration::from_millis(300));
    let mut waiter = test_run(&root)
        .args(["--timeout", "20"])
        .arg("--")
        .arg("sleep")
        .arg("20")
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn waiter");
    std::thread::sleep(Duration::from_millis(500));

    unsafe { libc::kill(waiter.id() as libc::c_int, libc::SIGTERM) };
    let start = Instant::now();
    let status = waiter.wait().expect("wait for the interrupted waiter");
    assert_eq!(status.code(), Some(143), "SIGTERM must exit 143");
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "the stop must follow the signal within 2s, took {:?}",
        start.elapsed()
    );
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut waiter.stderr.take().unwrap(), &mut stderr).unwrap();
    let line = stderr
        .lines()
        .find(|l| l.contains("suite_wait_interrupted"))
        .unwrap_or_else(|| panic!("interrupted receipt missing: {stderr}"));
    assert!(line.contains("signal=15"), "{line}");
    assert!(line.contains("waited_s="), "{line}");
    assert!(!stderr.contains("suite_started"), "the argv must never run");

    let _ = holder.kill();
    let _ = holder.wait();
    let _ = std::fs::remove_dir_all(&root);
}

/// AC5-ERR: SIGINT stops a queued waiter with exit 130, and the argv never
/// ran.
#[test]
fn a_queued_waiter_stops_on_sigint() {
    let root = tmp_claims_root("wait-int");
    let mut holder = test_run(&root)
        .args(["--timeout", "20"])
        .arg("--")
        .arg("sleep")
        .arg("20")
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn holder");
    std::thread::sleep(Duration::from_millis(300));
    let mut waiter = test_run(&root)
        .args(["--timeout", "20"])
        .arg("--")
        .arg("sleep")
        .arg("20")
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn waiter");
    std::thread::sleep(Duration::from_millis(500));

    unsafe { libc::kill(waiter.id() as libc::c_int, libc::SIGINT) };
    let start = Instant::now();
    let status = waiter.wait().expect("wait for the interrupted waiter");
    assert_eq!(status.code(), Some(130), "SIGINT must exit 130");
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "the stop must follow the signal within 2s, took {:?}",
        start.elapsed()
    );
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut waiter.stderr.take().unwrap(), &mut stderr).unwrap();
    assert!(
        stderr.contains("suite_wait_interrupted"),
        "interrupted receipt missing: {stderr}"
    );
    assert!(
        !stderr.contains("suite_started"),
        "the argv must never run: {stderr}"
    );

    let _ = holder.kill();
    let _ = holder.wait();
    let _ = std::fs::remove_dir_all(&root);
}

/// AC6-HP: a SIGKILLed holder's slot frees at once. The stamped
/// holder-process lease reads the dead pid as the verdict, so the next
/// waiter acquires inside 3s instead of timing out against the dead pid.
#[test]
fn a_dead_holders_slot_frees_at_once() {
    let root = tmp_claims_root("dead-holder");
    let mut holder = test_run(&root)
        .args(["--timeout", "20"])
        .arg("--")
        .arg("sleep")
        .arg("20")
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn holder");
    std::thread::sleep(Duration::from_millis(300));
    holder.kill().expect("SIGKILL the holder");
    let _ = holder.wait();

    let start = Instant::now();
    let status = test_run(&root)
        .args(["--timeout", "10"])
        .arg("--")
        .arg("true")
        .status()
        .expect("run the next waiter");
    assert!(status.success(), "the next run must acquire, got {status}");
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "a dead holder must free the slot at once, took {:?}",
        start.elapsed()
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// AC2-ERR: a run admitted behind a holder dies at its own run budget with a
/// TIMEOUT line that names the run seconds first and the queued seconds as
/// the parenthetical, then a line naming the budget lever and the narrow
/// target.
#[test]
fn the_run_timeout_names_the_wait_and_the_run() {
    let root = tmp_claims_root("timeout-split");
    let mut holder = test_run(&root)
        .args(["--timeout", "30"])
        .arg("--")
        .arg("sleep")
        .arg("2")
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn holder");
    std::thread::sleep(Duration::from_millis(300));
    let out = test_run(&root)
        .args(["--timeout", "2"])
        .arg("--")
        .arg("sleep")
        .arg("10")
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("run the late-admitted waiter");
    assert!(
        holder.try_wait().unwrap().is_some(),
        "the holder must have exited"
    );
    let _ = holder.wait();
    assert_eq!(out.status.code(), Some(124), "the deadline must exit 124");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let line = stderr
        .lines()
        .find(|l| l.contains("TIMEOUT after 2s running the argv"))
        .unwrap_or_else(|| panic!("timeout line missing: {stderr}"));
    assert!(line.contains("process group killed"), "{line}");
    let queued_secs: u64 = line
        .split('(')
        .nth(1)
        .and_then(|rest| rest.split("s queued").next())
        .unwrap_or("0")
        .trim()
        .parse()
        .unwrap_or(0);
    assert!(queued_secs >= 1, "the queued share must be named: {line}");
    let remedy = stderr
        .lines()
        .find(|l| l.contains("FNO_TEST_TIMEOUT_SECONDS"))
        .unwrap_or_else(|| panic!("remedy line missing: {stderr}"));
    assert!(remedy.contains("narrow the target"), "{remedy}");
    let _ = std::fs::remove_dir_all(&root);
}

/// AC1-HP: a waiter queued 3s behind a live holder keeps its whole budget
/// once admitted: `--timeout 5 -- sleep 4` runs its full 4 seconds and exits
/// 0, where the old shared deadline would have killed it 2s into the run.
#[test]
fn a_queued_run_keeps_its_whole_budget() {
    let root = tmp_claims_root("whole-budget");
    let mut holder = test_run(&root)
        .args(["--timeout", "30"])
        .arg("--")
        .arg("sleep")
        .arg("3")
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn holder");
    std::thread::sleep(Duration::from_millis(300));
    let out = test_run(&root)
        .args(["--timeout", "5"])
        .arg("--")
        .arg("sleep")
        .arg("4")
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("run the queued waiter");
    assert!(holder.try_wait().unwrap().is_some());
    let _ = holder.wait();
    assert!(
        out.status.success(),
        "the waiter must run its whole 4s budget, got {:?}: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("suite_wait_timeout"),
        "the wait has no timer: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// AC4-HP: a waiter whose own budget is already spent keeps waiting, is
/// admitted when the holder releases, and exits 0.
#[test]
fn a_waiter_outlasts_its_own_budget_while_the_holder_runs() {
    let root = tmp_claims_root("outlast-budget");
    let mut holder = test_run(&root)
        .args(["--timeout", "30"])
        .arg("--")
        .arg("sleep")
        .arg("4")
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn holder");
    std::thread::sleep(Duration::from_millis(300));
    let out = test_run(&root)
        .args(["--timeout", "1"])
        .arg("--")
        .arg("true")
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("run the under-budgeted waiter");
    assert!(holder.try_wait().unwrap().is_some());
    let _ = holder.wait();
    assert!(
        out.status.success(),
        "a queued waiter outlives its own budget: got {:?}: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// AC5-HP: the waiting line carries holder_left_s in the holder's remaining
/// budget, and the holder's own claim record sets expires_at to acquired_at
/// plus its --timeout. The budget clears the store's 60s TTL floor, so the
/// equality is exact.
#[test]
fn the_waiting_line_names_the_holders_remaining_budget() {
    let root = tmp_claims_root("holder-left");
    let mut holder = test_run(&root)
        .args(["--timeout", "90"])
        .arg("--")
        .arg("sleep")
        .arg("20")
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn holder");
    std::thread::sleep(Duration::from_millis(300));
    let waiter = test_run(&root)
        .args(["--timeout", "30"])
        .arg("--")
        .arg("true")
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the waiter");
    // The record is read while the holder still holds it; once the holder
    // releases, the record is gone.
    std::thread::sleep(Duration::from_millis(1500));
    let (_, rec) = fno_agents::claims::status("test:suite", Some(&root));
    let rec = rec.expect("the holder's claim record while it holds");
    assert_eq!(
        rec.expires_at,
        Some(rec.acquired_at + 90_000),
        "expires_at is the holder's own run deadline: {rec:?}"
    );
    let out = waiter.wait_with_output().expect("wait for the waiter");
    assert!(holder.try_wait().unwrap().is_some());
    let _ = holder.wait();
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    let line = stderr
        .lines()
        .find(|l| l.contains("suite_waiting") && l.contains("holder_left_s="))
        .unwrap_or_else(|| panic!("waiting line missing: {stderr}"));
    let left: i64 = line
        .split(" holder_left_s=")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .unwrap_or("")
        .parse()
        .unwrap_or(-1);
    assert!(
        (85..=90).contains(&left),
        "holder_left_s must sit in the holder's remaining 90s budget: {line}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// AC7-ERR: a fleet stop written while a waiter queues is read at its
/// ADMISSION, not only at entry: the waiter exits 90 after the holder
/// releases, names fleet-stop, never spawns, and a clear reopens admission.
#[test]
fn a_waiter_admitted_after_a_fleet_stop_refuses() {
    let root = tmp_claims_root("fleet-recheck");
    let home = tmp_claims_root("fleet-recheck-home");
    let mut holder = test_run(&root)
        .env("FNO_AGENTS_HOME", &home)
        .args(["--timeout", "30"])
        .arg("--")
        .arg("sleep")
        .arg("3")
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn holder");
    std::thread::sleep(Duration::from_millis(300));
    let mut waiter = test_run(&root)
        .env("FNO_AGENTS_HOME", &home)
        .args(["--timeout", "30"])
        .arg("--")
        .arg("sleep")
        .arg("60")
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the queued waiter");
    std::thread::sleep(Duration::from_millis(500));
    let stopped = Command::new(bin())
        .args(["fleet-incident", "stop", "--reason", "queue wedge"])
        .env("FNO_AGENTS_HOME", &home)
        .output()
        .expect("write the fleet stop");
    assert!(stopped.status.success(), "the stop must land");

    let start = Instant::now();
    let status = waiter.wait().expect("wait for the refused waiter");
    assert_eq!(
        status.code(),
        Some(90),
        "a waiter admitted after a fleet stop must refuse with 90"
    );
    assert!(
        start.elapsed() < Duration::from_secs(15),
        "the refusal must follow the holder's release, took {:?}",
        start.elapsed()
    );
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut waiter.stderr.take().unwrap(), &mut stderr).unwrap();
    assert!(stderr.contains("suite_refused"), "{stderr}");
    assert!(stderr.contains("fleet-stop"), "{stderr}");
    assert!(stderr.contains("waited_s="), "{stderr}");
    assert!(
        !stderr.contains("suite_started"),
        "the argv must never spawn mid-incident: {stderr}"
    );

    let _ = holder.wait();
    let cleared = Command::new(bin())
        .args(["fleet-incident", "clear", "--reason", "queue wedge done"])
        .env("FNO_AGENTS_HOME", &home)
        .output()
        .expect("clear the fleet stop");
    assert!(cleared.status.success(), "the clear must land");
    let third = test_run(&root)
        .env("FNO_AGENTS_HOME", &home)
        .args(["--timeout", "10"])
        .arg("--")
        .arg("true")
        .status()
        .expect("run after the clear");
    assert!(third.success(), "a clear reopens admission");
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&home);
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

/// A fake cargo holder: an `sh` whose only child execs a symlink to
/// `/bin/sleep`, so the census sees one child with a chosen argv0. Its own
/// process group lets the test killpg the whole holder at the end.
fn spawn_holder(root: &std::path::Path, dir: &str, name: &str) -> std::process::Child {
    let link_dir = root.join(dir);
    std::fs::create_dir_all(&link_dir).unwrap();
    let link = link_dir.join(name);
    std::os::unix::fs::symlink("/bin/sleep", &link).expect("symlink the chosen argv0");
    Command::new("/bin/sh")
        .arg("-c")
        .arg("\"$0\" 60; true")
        .arg(&link)
        .process_group(0)
        .spawn()
        .expect("spawn the holder sh")
}

fn kill_group(child: &mut std::process::Child) {
    unsafe { libc::killpg(child.id() as libc::c_int, libc::SIGTERM) };
    child.wait().unwrap();
}

/// A holder cargo whose only child is a test binary yields `build:cargo` to
/// a waiter after the idle window, while the holder still lives. The
/// takeover names the idle holder in the new claim's reason, the waiter's
/// stderr, and the waiter worktree's `claim_released` event.
#[test]
fn a_holder_in_its_test_phase_yields_the_slot_after_the_idle_window() {
    let root = std::fs::canonicalize(tmp_claims_root("idle-takeover")).unwrap();
    let (tree_a, tree_b) = (root.join("a"), root.join("b"));
    std::fs::create_dir_all(&tree_a).unwrap();
    std::fs::create_dir_all(&tree_b).unwrap();
    let mut holder = spawn_holder(&root, "target/debug/deps", "fno_agents-0123abcd");

    let first = build_admit(&root, holder.id(), &tree_a).status().unwrap();
    assert!(first.success(), "the holder must be admitted at once");
    let old_holder = build_holder(&root).expect("the holder cargo holds build:cargo");

    let mut cargo_b = Command::new("sleep").arg("60").spawn().unwrap();
    let mut waiter = build_admit(&root, cargo_b.id(), &tree_b)
        .env("FNO_TEST_BUILD_IDLE_SECS", "1")
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    let started = Instant::now();
    let status = loop {
        match waiter.try_wait().unwrap() {
            Some(status) => break status,
            None => {
                assert!(
                    started.elapsed() < Duration::from_secs(12),
                    "the waiter must take over within the idle window plus one scan"
                );
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    };
    assert!(
        status.success(),
        "the takeover must read as success, got {status}"
    );
    assert!(
        pid_alive(holder.id()),
        "the idle holder must still be alive at takeover"
    );

    let new_holder = build_holder(&root).expect("the waiter holds build:cargo after takeover");
    assert!(
        new_holder.contains(tree_b.to_str().unwrap()),
        "the claim must name the waiter's tree: {new_holder}"
    );
    let (_, rec) = fno_agents::claims::status("build:cargo", Some(&root));
    let rec = rec.expect("a claim record after takeover");
    assert!(
        rec.reason
            .as_deref()
            .is_some_and(|r| r.contains("took over from") && r.contains(&old_holder)),
        "the reason must name the idle holder: {:?}",
        rec.reason
    );

    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut waiter.stderr.take().unwrap(), &mut stderr).unwrap();
    assert!(
        stderr.contains("cargo admission: taking over") && stderr.contains(&old_holder),
        "stderr must name the takeover and the idle holder: {stderr}"
    );

    // The claims lifecycle is an ephemeral event class: it lands in the
    // sibling journal beside .fno/events.jsonl.
    let events = fno_agents::event_store::journal_text(&tree_b.join(".fno/events.jsonl"), &[]);
    assert!(
        events.contains("claim_released") && events.contains(&old_holder),
        "the events journal must name the released idle holder"
    );

    kill_group(&mut holder);
    let _ = cargo_b.kill();
    let _ = cargo_b.wait();
    let _ = std::fs::remove_dir_all(&root);
}

/// The control that separates takeover from a TTL: a holder with a live
/// compile process under it keeps the slot past the same window, and the
/// waiter is admitted within 2s of the holder's death.

#[test]
fn a_compiling_holder_keeps_the_slot_past_the_idle_window() {
    let root = std::fs::canonicalize(tmp_claims_root("compile-keeps")).unwrap();
    let (tree_a, tree_b) = (root.join("a"), root.join("b"));
    std::fs::create_dir_all(&tree_a).unwrap();
    std::fs::create_dir_all(&tree_b).unwrap();
    let mut holder = spawn_holder(&root, "bin", "rustc");
    // Let the compile-process child appear before the first idle scan.
    std::thread::sleep(Duration::from_millis(300));

    let first = build_admit(&root, holder.id(), &tree_a).status().unwrap();
    assert!(first.success(), "the holder must be admitted at once");
    let old_holder = build_holder(&root).expect("the holder cargo holds build:cargo");

    let mut cargo_b = Command::new("sleep").arg("60").spawn().unwrap();
    let mut waiter = build_admit(&root, cargo_b.id(), &tree_b)
        .env("FNO_TEST_BUILD_IDLE_SECS", "1")
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    std::thread::sleep(Duration::from_secs(8));
    assert!(
        waiter.try_wait().unwrap().is_none(),
        "a compiling holder keeps the slot past the idle window"
    );

    kill_group(&mut holder);
    let released = Instant::now();
    let status = waiter.wait().unwrap();
    assert!(
        status.success(),
        "the waiter must follow the holder, got {status}"
    );
    assert!(
        released.elapsed() < Duration::from_secs(2),
        "admission must follow the holder's death within 2s, took {:?}",
        released.elapsed()
    );
    assert_ne!(
        build_holder(&root),
        Some(old_holder),
        "the waiter now holds"
    );
    let _ = cargo_b.kill();
    let _ = cargo_b.wait();
    let _ = std::fs::remove_dir_all(&root);
}

fn slot_pool_setup(root: &std::path::Path) {
    std::fs::write(root.join("config.toml"), "[test]\nmax_cargo_runs = 2\n")
        .expect("write the slot-pool config");
}

fn run_admit(root: &std::path::Path, cargo_pid: u32, worktree: &std::path::Path) -> Command {
    let mut cmd = Command::new(bin());
    cmd.args(["test-run", "run-admit", "--cargo-pid"])
        .arg(cargo_pid.to_string())
        .arg("--worktree")
        .arg(worktree)
        .env("FNO_CLAIMS_ROOT", root)
        .env("TMPDIR", root)
        .env("FNO_CONFIG", root.join("config.toml"));
    cmd
}

/// AC1-HP: two cargos hold both run slots; a third waits, names both
/// holders and the count, and takes the freed slot within 2s.
#[test]
fn a_third_cargo_run_waits_until_a_slot_frees() {
    let root = std::fs::canonicalize(tmp_claims_root("run-pool")).unwrap();
    let (tree_1, tree_2, tree_3) = (root.join("w1"), root.join("w2"), root.join("w3"));
    for tree in [&tree_1, &tree_2, &tree_3] {
        std::fs::create_dir_all(tree).unwrap();
    }
    slot_pool_setup(&root);

    let mut holder_1 = Command::new("sleep").arg("60").spawn().unwrap();
    let mut holder_2 = Command::new("sleep").arg("60").spawn().unwrap();
    let first = run_admit(&root, holder_1.id(), &tree_1).status().unwrap();
    assert!(first.success(), "slot 0 must be taken at once");
    let second = run_admit(&root, holder_2.id(), &tree_2).status().unwrap();
    assert!(second.success(), "slot 1 must be taken at once");

    let mut waiter = run_admit(&root, std::process::id(), &tree_3)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        waiter.try_wait().unwrap().is_none(),
        "the third cargo must wait while both slots are held"
    );

    let _ = holder_1.kill();
    let _ = holder_1.wait();
    let released = Instant::now();
    let status = waiter.wait().unwrap();
    assert!(
        status.success(),
        "the waiter must be admitted, got {status}"
    );
    assert!(
        released.elapsed() < Duration::from_secs(2),
        "admission must follow the freed slot within 2s, took {:?}",
        released.elapsed()
    );
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut waiter.stderr.take().unwrap(), &mut stderr).unwrap();
    assert!(
        stderr.contains("cargo admission: holding")
            && stderr.contains("2 of 2 cargo run slots held by"),
        "stderr must name the pool count: {stderr}"
    );
    assert!(
        stderr.contains(&format!("cargo:{}:{}", tree_1.display(), holder_1.id()))
            && stderr.contains(&format!("cargo:{}:{}", tree_2.display(), holder_2.id())),
        "stderr must name both holders: {stderr}"
    );
    let _ = holder_2.kill();
    let _ = holder_2.wait();
    let _ = std::fs::remove_dir_all(&root);
}

/// AC3-EDGE: a process under a slot holder (a nested cargo, a doctest's
/// rustdoc) is admitted through the status pass with no second claim.
#[test]
fn a_process_under_a_slot_holder_is_admitted_without_a_second_slot() {
    let root = std::fs::canonicalize(tmp_claims_root("run-nested")).unwrap();
    let tree_1 = root.join("w1");
    std::fs::create_dir_all(&tree_1).unwrap();
    slot_pool_setup(&root);

    let first = run_admit(&root, std::process::id(), &tree_1)
        .status()
        .unwrap();
    assert!(first.success(), "the test process must take slot 0 at once");

    let mut child = Command::new("sleep").arg("60").spawn().unwrap();
    let start = Instant::now();
    let status = run_admit(&root, child.id(), &root.join("w2"))
        .status()
        .unwrap();
    assert!(status.success(), "the child must be admitted, got {status}");
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
    let (_, rec) = fno_agents::claims::status("test:cargo-run:1", Some(&root));
    assert!(
        !matches!(rec, Some(_)),
        "slot 1 must stay free: a nested ask writes no second claim"
    );
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&root);
}

/// AC4-HP: with every slot held, a waiting build-admit leaves build:cargo
/// without a holder; it takes the slot first, then the build claim.
#[test]
fn build_admit_takes_a_run_slot_before_build_cargo() {
    let root = std::fs::canonicalize(tmp_claims_root("run-order")).unwrap();
    let (tree_1, tree_b) = (root.join("w1"), root.join("wb"));
    std::fs::create_dir_all(&tree_1).unwrap();
    std::fs::create_dir_all(&tree_b).unwrap();
    slot_pool_setup(&root);

    let mut holder_1 = Command::new("sleep").arg("60").spawn().unwrap();
    let mut holder_2 = Command::new("sleep").arg("60").spawn().unwrap();
    assert!(run_admit(&root, holder_1.id(), &tree_1)
        .status()
        .unwrap()
        .success());
    assert!(run_admit(&root, holder_2.id(), &root.join("w2"))
        .status()
        .unwrap()
        .success());

    let mut cargo_b = Command::new("sleep").arg("60").spawn().unwrap();
    let mut waiter = build_admit(&root, cargo_b.id(), &tree_b)
        .env("FNO_CONFIG", root.join("config.toml"))
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        waiter.try_wait().unwrap().is_none(),
        "the build waiter must wait"
    );
    assert!(
        build_holder(&root).is_none(),
        "build:cargo must stay unheld while every slot is held"
    );

    let _ = holder_1.kill();
    let _ = holder_1.wait();
    let status = waiter.wait().unwrap();
    assert!(
        status.success(),
        "the build waiter must be admitted, got {status}"
    );
    let new_holder = build_holder(&root).expect("the build waiter now holds build:cargo");
    assert!(
        new_holder.contains(&cargo_b.id().to_string()),
        "build:cargo must name the waiter's cargo: {new_holder}"
    );
    let _ = cargo_b.kill();
    let _ = cargo_b.wait();
    let _ = holder_2.kill();
    let _ = holder_2.wait();
    let _ = std::fs::remove_dir_all(&root);
}

/// AC8-HP: a cargo waiting on a run slot writes the same stop-hook marker
/// as a build waiter; the stop hook's read names a holder.
#[test]
fn a_run_slot_waiter_writes_the_stop_hook_marker() {
    let root = std::fs::canonicalize(tmp_claims_root("run-marker")).unwrap();
    let tree_w = root.join("ww");
    std::fs::create_dir_all(&tree_w).unwrap();
    slot_pool_setup(&root);

    let mut holder_1 = Command::new("sleep").arg("60").spawn().unwrap();
    let mut holder_2 = Command::new("sleep").arg("60").spawn().unwrap();
    assert!(run_admit(&root, holder_1.id(), &root.join("w1"))
        .status()
        .unwrap()
        .success());
    assert!(run_admit(&root, holder_2.id(), &root.join("w2"))
        .status()
        .unwrap()
        .success());

    let mut waiter = run_admit(&root, std::process::id(), &tree_w)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(1));
    // The stop hook reads the global waiters dir; pin this test process to
    // the same claims root the waiter subprocesses used.
    std::env::set_var("FNO_CLAIMS_ROOT", &root);
    let message = fno_agents::test_run::build_hold_message(&tree_w)
        .expect("the waiting run slot writes the stop-hook marker");
    std::env::remove_var("FNO_CLAIMS_ROOT");
    assert!(
        message.contains("held for"),
        "the hold message names a holder: {message}"
    );

    let _ = holder_1.kill();
    let _ = holder_2.kill();
    let _ = holder_1.wait();
    let _ = holder_2.wait();
    let status = waiter.wait().unwrap();
    assert!(
        status.success(),
        "the waiter must be admitted, got {status}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// AC18-HP helper: name one checkout the priority lane through the same
/// claim surface the readout teaches (pid-unavailable acquire with a TTL).
fn set_priority(root: &std::path::Path, worktree: &std::path::Path, ttl_ms: i64) {
    let outcome = fno_agents::claims::acquire(
        fno_agents::test_run::PRIORITY_KEY,
        &format!("worktree:{}", worktree.display()),
        fno_agents::claims::AcquireOpts {
            ttl_ms: Some(ttl_ms),
            pid_unavailable: true,
            reason: Some("test lane".to_string()),
            root: Some(root.to_path_buf()),
            ..Default::default()
        },
    );
    assert!(
        matches!(outcome, fno_agents::claims::AcquireOutcome::Acquired(_)),
        "the priority lane must acquire"
    );
}

/// An executable named `cargo` that appends its label to an order file when
/// its argv runs: the suite classifier reads its basename, so lane logic is
/// observed with no real cargo.
fn fake_cargo(dir: &std::path::Path) -> std::path::PathBuf {
    let script = dir.join("cargo");
    std::fs::write(
        &script,
        "#!/bin/sh\necho \"$FAKE_LABEL\" >> \"$FAKE_ORDER\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    script
}

fn git_dir(path: &std::path::Path) {
    std::fs::create_dir_all(path.join(".git")).unwrap();
}

/// AC18-HP: a priority checkout takes the next free run slot ahead of an
/// earlier normal waiter; the earlier waiter names what it yields to.
#[test]
fn a_priority_checkout_jumps_the_run_slot_queue() {
    let root = std::fs::canonicalize(tmp_claims_root("prio-run")).unwrap();
    let (w1, w2, w3, w4) = (
        root.join("w1"),
        root.join("w2"),
        root.join("w3"),
        root.join("w4"),
    );
    for w in [&w1, &w2, &w3, &w4] {
        std::fs::create_dir_all(w).unwrap();
    }
    slot_pool_setup(&root);

    let mut h1 = Command::new("sleep").arg("60").spawn().unwrap();
    let mut h2 = Command::new("sleep").arg("60").spawn().unwrap();
    assert!(run_admit(&root, h1.id(), &w1).status().unwrap().success());
    assert!(run_admit(&root, h2.id(), &w2).status().unwrap().success());

    // Q from w3 queues first, before the lane exists.
    let mut q = run_admit(&root, std::process::id(), &w3)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(1));
    set_priority(&root, &w4, 600_000);
    // P from w4 queues second, in the priority lane.
    let mut p_proc = Command::new("sleep").arg("60").spawn().unwrap();
    let mut p = run_admit(&root, p_proc.id(), &w4)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(1));
    assert!(
        q.try_wait().unwrap().is_none() && p.try_wait().unwrap().is_none(),
        "both waiters must still wait while both slots are held"
    );

    // Free one slot: the priority waiter takes it.
    let _ = h1.kill();
    let _ = h1.wait();
    let start = Instant::now();
    let p_status = p.wait().unwrap();
    assert!(
        p_status.success() && start.elapsed() < Duration::from_secs(3),
        "P must be admitted ahead of Q within 3s, took {:?} ({p_status})",
        start.elapsed()
    );
    let mut p_err = String::new();
    std::io::Read::read_to_string(&mut p.stderr.take().unwrap(), &mut p_err).unwrap();
    assert!(
        p_err.contains("holding (priority lane)"),
        "P must name its lane: {p_err}"
    );

    // Q still waits and names the checkout it yields to.
    assert!(
        q.try_wait().unwrap().is_none(),
        "Q must keep waiting behind the priority checkout"
    );
    let mut q_err = String::new();
    std::io::Read::read_to_string(&mut q.stderr.take().unwrap(), &mut q_err).unwrap();
    assert!(
        q_err.contains("yielding to priority worktree") && q_err.contains(w4.to_str().unwrap()),
        "Q must name the priority checkout: {q_err}"
    );

    // Free the rest: Q is admitted.
    let _ = p_proc.kill();
    let _ = p_proc.wait();
    let _ = h2.kill();
    let _ = h2.wait();
    let start = Instant::now();
    let q_status = q.wait().unwrap();
    assert!(
        q_status.success() && start.elapsed() < Duration::from_secs(3),
        "Q must follow within 3s, took {:?} ({q_status})",
        start.elapsed()
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// AC19-EDGE: a live lane reserves nothing. With the lane held for w4 but no
/// waiter from w4, a normal waiter is admitted as soon as a slot frees.
#[test]
fn an_idle_priority_lane_reserves_nothing() {
    let root = std::fs::canonicalize(tmp_claims_root("prio-idle")).unwrap();
    let (w1, w2, w3, w4) = (
        root.join("w1"),
        root.join("w2"),
        root.join("w3"),
        root.join("w4"),
    );
    for w in [&w1, &w2, &w3, &w4] {
        std::fs::create_dir_all(w).unwrap();
    }
    slot_pool_setup(&root);

    let mut h1 = Command::new("sleep").arg("60").spawn().unwrap();
    let mut h2 = Command::new("sleep").arg("60").spawn().unwrap();
    assert!(run_admit(&root, h1.id(), &w1).status().unwrap().success());
    assert!(run_admit(&root, h2.id(), &w2).status().unwrap().success());

    set_priority(&root, &w4, 600_000);
    let mut q = run_admit(&root, std::process::id(), &w3)
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(1));
    assert!(q.try_wait().unwrap().is_none());

    let _ = h1.kill();
    let _ = h1.wait();
    let start = Instant::now();
    let status = q.wait().unwrap();
    assert!(
        status.success() && start.elapsed() < Duration::from_secs(3),
        "an idle lane must never hold a slot, took {:?}",
        start.elapsed()
    );
    let _ = h2.kill();
    let _ = h2.wait();
    let _ = std::fs::remove_dir_all(&root);
}

/// AC20-HP: at the suite door, a priority checkout's argv runs ahead of an
/// earlier targeted waiter, and both waiting lines name the lane read.
#[test]
fn the_priority_lane_orders_the_suite_door() {
    let root = std::fs::canonicalize(tmp_claims_root("prio-suite")).unwrap();
    let (w3, w4) = (root.join("w3"), root.join("w4"));
    git_dir(&w3);
    git_dir(&w4);
    let order = root.join("order.txt");
    let fake = fake_cargo(&root);

    let mut holder = test_run(&root)
        .args(["--timeout", "30"])
        .arg("--")
        .arg("sleep")
        .arg("30")
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));

    // Q from w3 queues first, targeted.
    let mut q = test_run(&root)
        .current_dir(&w3)
        .args(["--timeout", "30"])
        .arg("--")
        .arg(&fake)
        .args(["test", "--manifest-path", "x/Cargo.toml", "one_test"])
        .env("FAKE_LABEL", "Q")
        .env("FAKE_ORDER", &order)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(1));
    set_priority(&root, &w4, 600_000);
    // P from w4 queues second, in the priority lane.
    let mut p = test_run(&root)
        .current_dir(&w4)
        .args(["--timeout", "30"])
        .arg("--")
        .arg(&fake)
        .args(["test", "--manifest-path", "x/Cargo.toml", "one_test"])
        .env("FAKE_LABEL", "P")
        .env("FAKE_ORDER", &order)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(1));
    assert!(
        q.try_wait().unwrap().is_none() && p.try_wait().unwrap().is_none(),
        "both must wait behind the holder"
    );

    let _ = holder.kill();
    let _ = holder.wait();
    let start = Instant::now();
    let p_status = p.wait().unwrap();
    let q_status = q.wait().unwrap();
    assert!(p_status.success() && q_status.success());
    assert!(
        start.elapsed() < Duration::from_secs(6),
        "both must run after the holder dies, took {:?}",
        start.elapsed()
    );
    let order_text = std::fs::read_to_string(&order).unwrap();
    assert_eq!(
        order_text.lines().collect::<Vec<_>>(),
        vec!["P", "Q"],
        "the priority checkout's argv runs first: {order_text}"
    );
    let mut p_err = String::new();
    std::io::Read::read_to_string(&mut p.stderr.take().unwrap(), &mut p_err).unwrap();
    assert!(
        p_err.contains("lane=priority"),
        "P's waiting line names its lane: {p_err}"
    );
    let mut q_err = String::new();
    std::io::Read::read_to_string(&mut q.stderr.take().unwrap(), &mut q_err).unwrap();
    assert!(
        q_err.contains("yielding_to=priority") && q_err.contains(w4.to_str().unwrap()),
        "Q's waiting line names the lane and the checkout: {q_err}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A pid with no living ancestor inside this test process: the sleep is
/// backgrounded under a shell that exits, so init reparents it. A waiter
/// naming this pid is never ancestor-admitted by a claim the test process
/// holds.
fn detached_pid() -> u32 {
    let dir = std::env::temp_dir().join(format!(
        "fno-detach-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let pid_file = dir.join("pid");
    Command::new("/bin/sh")
        .arg("-c")
        .arg(format!(
            "sleep 60 & echo $! > {}; exit 0",
            pid_file.display()
        ))
        .status()
        .expect("spawn the detaching shell");
    let pid: u32 = std::fs::read_to_string(&pid_file)
        .expect("the shell must write the backgrounded pid")
        .trim()
        .parse()
        .expect("pid file must hold a bare pid");
    pid
}

fn kill_pid(pid: u32) {
    unsafe {
        libc::kill(pid as libc::c_int, libc::SIGKILL);
    }
}

/// AC21-EDGE: the ancestor admit survives the lane gate. With the test
/// process holding build:cargo and a priority waiter queued, a child ask is
/// admitted at once while the priority waiter still waits.
#[test]
fn the_lane_gate_keeps_the_ancestor_admit() {
    let root = std::fs::canonicalize(tmp_claims_root("lane-ancestor")).unwrap();
    let (outer, w2, w4) = (root.join("outer"), root.join("w2"), root.join("w4"));
    for w in [&outer, &w2, &w4] {
        std::fs::create_dir_all(w).unwrap();
    }

    let first = build_admit(&root, std::process::id(), &outer)
        .status()
        .unwrap();
    assert!(first.success(), "the test process holds build:cargo");
    set_priority(&root, &w4, 600_000);

    let p_pid = detached_pid();
    let mut p = build_admit(&root, p_pid, &w4)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(1));

    let mut child = Command::new("sleep").arg("60").spawn().unwrap();
    let start = Instant::now();
    let child_ask = build_admit(&root, child.id(), &w2).status().unwrap();
    assert!(
        child_ask.success() && start.elapsed() < Duration::from_secs(2),
        "a child of the holder must be admitted while the lane gates: took {:?}",
        start.elapsed()
    );

    kill_pid(p_pid);
    let _ = child.kill();
    let _ = child.wait();
    let _ = p.wait();
    let mut p_err = String::new();
    std::io::Read::read_to_string(&mut p.stderr.take().unwrap(), &mut p_err).unwrap();
    assert!(
        p_err.contains("holding (priority lane)"),
        "the priority waiter must keep waiting: {p_err}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// AC22-HP: the build door queues in arrival order. A admitted, B still
/// waiting, with two tickets in the queue dir during the wait.
#[test]
fn the_build_door_queues_in_arrival_order() {
    let root = std::fs::canonicalize(tmp_claims_root("build-order")).unwrap();
    let (outer, wa, wb) = (root.join("outer"), root.join("wa"), root.join("wb"));
    for w in [&outer, &wa, &wb] {
        std::fs::create_dir_all(w).unwrap();
    }
    // Every build-door waiter first takes a run slot (the fixed lock
    // order), so the pool must fit the holder and both waiters or the
    // second waiter wedges at the slot door and never queues here.
    std::fs::write(root.join("config.toml"), "[test]\nmax_cargo_runs = 4\n")
        .expect("write the slot-pool config");
    let slots = || {
        std::iter::once(("FNO_CONFIG", root.join("config.toml")))
            .collect::<std::collections::HashMap<_, _>>()
    };

    let holder_pid = detached_pid();
    let first = build_admit(&root, holder_pid, &outer)
        .envs(slots())
        .status()
        .unwrap();
    assert!(first.success(), "the detached holder holds build:cargo");

    let mut proc_a = Command::new("sleep").arg("60").spawn().unwrap();
    let mut proc_b = Command::new("sleep").arg("60").spawn().unwrap();
    let mut a = build_admit(&root, proc_a.id(), &wa)
        .envs(slots())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(1));
    let mut b = build_admit(&root, proc_b.id(), &wb)
        .envs(slots())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(1));

    let queue_dir = root
        .join(".fno")
        .join("claims")
        .join("build%3Acargo.lock.queue.d");
    let tickets = std::fs::read_dir(&queue_dir)
        .expect("the queue dir must exist once waiters queue")
        .count();
    assert_eq!(tickets, 2, "both waiters hold tickets: {queue_dir:?}");

    // Kill the holder: its dead pid frees the claim, A is admitted first.
    kill_pid(holder_pid);
    let start = Instant::now();
    let a_status = a.wait().unwrap();
    assert!(
        a_status.success() && start.elapsed() < Duration::from_secs(3),
        "A must be admitted first within 3s, took {:?}",
        start.elapsed()
    );
    assert!(
        b.try_wait().unwrap().is_none(),
        "B must still wait behind A"
    );

    let _ = fno_agents::claims::release(
        "build:cargo",
        &format!("cargo:{}:{}", wa.display(), proc_a.id()),
        Some(&root),
        None,
    );
    let start = Instant::now();
    let b_status = b.wait().unwrap();
    assert!(
        b_status.success() && start.elapsed() < Duration::from_secs(3),
        "B must follow within 3s, took {:?}",
        start.elapsed()
    );
    let _ = proc_a.kill();
    let _ = proc_a.wait();
    let _ = proc_b.kill();
    let _ = proc_b.wait();
    let _ = std::fs::remove_dir_all(&root);
}

/// AC23-EDGE: a waiter queued ahead at the build door never blocks the
/// holder's own child: the child is admitted from the back of the queue.
#[test]
fn the_ancestor_admit_passes_a_queued_waiter() {
    let root = std::fs::canonicalize(tmp_claims_root("ancestor-back")).unwrap();
    let (outer, ww, w2) = (root.join("outer"), root.join("ww"), root.join("w2"));
    for w in [&outer, &ww, &w2] {
        std::fs::create_dir_all(w).unwrap();
    }

    let first = build_admit(&root, std::process::id(), &outer)
        .status()
        .unwrap();
    assert!(first.success());

    let w_pid = detached_pid();
    let mut w = build_admit(&root, w_pid, &ww)
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(1));

    let mut child = Command::new("sleep").arg("60").spawn().unwrap();
    let start = Instant::now();
    let ask = build_admit(&root, child.id(), &w2).status().unwrap();
    assert!(
        ask.success() && start.elapsed() < Duration::from_secs(2),
        "the child must pass the queued waiter, took {:?}",
        start.elapsed()
    );

    kill_pid(w_pid);
    let _ = child.kill();
    let _ = child.wait();
    let _ = w.wait();
    let _ = std::fs::remove_dir_all(&root);
}

/// AC24-HP: at the suite door a whole-suite waiter yields to a targeted
/// waiter that queued after it, prints lane=full + yielding_to=normal and
/// the one whole-suite notice, and runs after the targeted argv.
#[test]
fn a_whole_suite_waiter_yields_to_a_targeted_run() {
    let root = std::fs::canonicalize(tmp_claims_root("full-lane")).unwrap();
    let order = root.join("order.txt");
    let fake = fake_cargo(&root);

    let mut holder = test_run(&root)
        .args(["--timeout", "30"])
        .arg("--")
        .arg("sleep")
        .arg("30")
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));

    // F: whole-suite argv, no filter, queues first.
    let mut f = test_run(&root)
        .args(["--timeout", "30"])
        .arg("--")
        .arg(&fake)
        .args(["test", "--manifest-path", "x/Cargo.toml"])
        .env("FAKE_LABEL", "F")
        .env("FAKE_ORDER", &order)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(1));
    // T: targeted argv, queues second.
    let mut t = test_run(&root)
        .args(["--timeout", "30"])
        .arg("--")
        .arg(&fake)
        .args(["test", "--manifest-path", "x/Cargo.toml", "one_test"])
        .env("FAKE_LABEL", "T")
        .env("FAKE_ORDER", &order)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(1));
    assert!(f.try_wait().unwrap().is_none() && t.try_wait().unwrap().is_none());

    let _ = holder.kill();
    let _ = holder.wait();
    let start = Instant::now();
    let t_status = t.wait().unwrap();
    let f_status = f.wait().unwrap();
    assert!(t_status.success() && f_status.success());
    assert!(
        start.elapsed() < Duration::from_secs(6),
        "both must run once the slot frees, took {:?}",
        start.elapsed()
    );
    let order_text = std::fs::read_to_string(&order).unwrap();
    assert_eq!(
        order_text.lines().collect::<Vec<_>>(),
        vec!["T", "F"],
        "the targeted run goes first: {order_text}"
    );
    let f_err = String::new();
    let mut f_err = f_err;
    std::io::Read::read_to_string(&mut f.stderr.take().unwrap(), &mut f_err).unwrap();
    assert!(f_err.contains("lane=full"), "{f_err}");
    assert!(f_err.contains("yielding_to=normal"), "{f_err}");
    assert_eq!(
        f_err.matches("whole crate suite").count(),
        1,
        "exactly one whole-suite notice: {f_err}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// AC25-EDGE: a whole-suite waiter yields for at most its own budget, then
/// joins the normal queue at the back: waiters that queued before the flip
/// still go first, later arrivals go after.
#[test]
fn a_whole_suite_waiter_joins_the_back_after_its_budget() {
    let root = std::fs::canonicalize(tmp_claims_root("yield-cap")).unwrap();
    let order = root.join("order.txt");
    let fake = fake_cargo(&root);

    let mut holder = test_run(&root)
        .args(["--timeout", "30"])
        .arg("--")
        .arg("sleep")
        .arg("8")
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));

    // F: whole argv with a 2s budget, queued first.
    let mut f = test_run(&root)
        .args(["--timeout", "2"])
        .arg("--")
        .arg(&fake)
        .args(["test", "--manifest-path", "x/Cargo.toml"])
        .env("FAKE_LABEL", "F")
        .env("FAKE_ORDER", &order)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    // T1: targeted, queued before the flip.
    let mut t1 = test_run(&root)
        .args(["--timeout", "30"])
        .arg("--")
        .arg(&fake)
        .args(["test", "--manifest-path", "x/Cargo.toml", "one_test"])
        .env("FAKE_LABEL", "T1")
        .env("FAKE_ORDER", &order)
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(3));
    // T2: targeted, queued after F's flip.
    let mut t2 = test_run(&root)
        .args(["--timeout", "30"])
        .arg("--")
        .arg(&fake)
        .args(["test", "--manifest-path", "x/Cargo.toml", "one_test"])
        .env("FAKE_LABEL", "T2")
        .env("FAKE_ORDER", &order)
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    let start = Instant::now();
    let f_status = f.wait().unwrap();
    let t1_status = t1.wait().unwrap();
    let t2_status = t2.wait().unwrap();
    assert!(f_status.success() && t1_status.success() && t2_status.success());
    let _ = holder.wait();
    let order_text = std::fs::read_to_string(&order).unwrap();
    let ran: Vec<&str> = order_text.lines().collect();
    let f_idx = ran
        .iter()
        .position(|l| *l == "F")
        .unwrap_or_else(|| panic!("F must run: {order_text}"));
    let t2_idx = ran
        .iter()
        .position(|l| *l == "T2")
        .unwrap_or_else(|| panic!("T2 must run: {order_text}"));
    let t1_idx = ran
        .iter()
        .position(|l| *l == "T1")
        .unwrap_or_else(|| panic!("T1 must run: {order_text}"));
    assert!(
        t1_idx < f_idx && f_idx < t2_idx,
        "T1 (queued before the flip) then F then T2: {order_text}"
    );
    let f_err = String::new();
    let mut f_err = f_err;
    std::io::Read::read_to_string(&mut f.stderr.take().unwrap(), &mut f_err).unwrap();
    assert!(
        f_err.contains("lane=normal"),
        "after the budget F waits in the normal lane: {f_err}"
    );
    assert!(start.elapsed() < Duration::from_secs(12), "{start:?}");
    let _ = std::fs::remove_dir_all(&root);
}

/// AC26-EDGE: with nothing queued, a whole-suite run is admitted at once.
/// The full lane delays a whole run only while a targeted run waits.
#[test]
fn an_uncontended_whole_suite_run_is_admitted_at_once() {
    let root = tmp_claims_root("full-solo");
    let order = root.join("order.txt");
    let fake = fake_cargo(&root);
    let out = test_run(&root)
        .args(["--timeout", "5"])
        .arg("--")
        .arg(&fake)
        .args(["test", "--manifest-path", "x/Cargo.toml"])
        .env("FAKE_LABEL", "F")
        .env("FAKE_ORDER", &order)
        .output()
        .expect("run the whole-suite argv");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let order_text = std::fs::read_to_string(&order).unwrap();
    assert_eq!(order_text.trim(), "F", "the argv ran at once");
    let _ = std::fs::remove_dir_all(&root);
}
