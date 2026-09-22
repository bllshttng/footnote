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

/// AC8-HP: a run admitted late dies at the shared deadline with a TIMEOUT
/// line that names the split: seconds waiting for the claim, seconds
/// running the argv.
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
        .args(["--timeout", "4"])
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
        .find(|l| l.contains("TIMEOUT after 4s"))
        .unwrap_or_else(|| panic!("timeout line missing: {stderr}"));
    assert!(
        line.contains("waiting for the test:suite claim") && line.contains("running the argv"),
        "{line}"
    );
    let wait_secs: u64 = line
        .split("TIMEOUT after 4s: ")
        .nth(1)
        .and_then(|rest| rest.split("s waiting").next())
        .unwrap_or("0")
        .trim()
        .parse()
        .unwrap_or(0);
    assert!(wait_secs >= 1, "the wait share must be named: {line}");
    let run_secs: u64 = line
        .split("s waiting for the test:suite claim, ")
        .nth(1)
        .and_then(|rest| rest.split("s running").next())
        .unwrap_or("0")
        .trim()
        .parse()
        .unwrap_or(0);
    assert!(run_secs >= 1, "the run share must be named: {line}");
    let _ = std::fs::remove_dir_all(&root);
}

/// AC9-HP: a waiter that never reaches the front ends at the budget with the
/// refusal naming how many runs were ahead of it and that the argv never
/// started.
#[test]
fn the_wait_refusal_names_the_queue_and_the_unstarted_argv() {
    let root = tmp_claims_root("wait-refusal");
    let mut holder = test_run(&root)
        .args(["--timeout", "20"])
        .arg("--")
        .arg("sleep")
        .arg("20")
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn holder");
    std::thread::sleep(Duration::from_millis(300));
    let out = test_run(&root)
        .args(["--timeout", "2"])
        .arg("--")
        .arg("true")
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("run the never-admitted waiter");
    assert_eq!(out.status.code(), Some(124));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("The argv never started."), "{stderr}");
    assert!(
        stderr.contains("runs were ahead of it in a queue of"),
        "{stderr}"
    );

    let _ = holder.kill();
    let _ = holder.wait();
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
