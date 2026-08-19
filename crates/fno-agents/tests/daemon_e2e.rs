//! End-to-end daemon tests with real subprocesses (Wave 3, Discretion #5).
//!
//! These are the load-bearing proofs of the wave: a PTY-managed agent survives
//! the daemon's death (Outcome B), and a restarted daemon reconnects to the
//! still-live worker via socket discovery. No monkeypatching — real `daemon`,
//! `worker`, and `sleep` processes.

use fno_agents::client::call;
use fno_agents::paths::AgentsHome;
use fno_agents::protocol::Request;
use fno_agents::state;
use serde_json::json;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const DAEMON_BIN: &str = env!("CARGO_BIN_EXE_fno-agents-daemon");
const WORKER_BIN: &str = env!("CARGO_BIN_EXE_fno-agents-worker");
const CLIENT_BIN: &str = env!("CARGO_BIN_EXE_fno-agents");

/// Short home root (Unix-socket `sun_path` is ~104 bytes; `/var/folders/...` is
/// too long on macOS).
fn short_home() -> AgentsHome {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    // pid + counter: collision-proof across parallel tests, and short enough
    // for the Unix-socket sun_path limit.
    AgentsHome::at(PathBuf::from(format!(
        "/tmp/fnoe{}_{}",
        std::process::id(),
        n
    )))
}

fn pid_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// Spawn the daemon as a tracked child (so the test holds its PID) and wait for
/// the socket. The worker-bin override is passed through the env.
fn start_daemon(home: &AgentsHome) -> std::process::Child {
    start_daemon_env(home, &[])
}

/// Like [`start_daemon`] but with extra env on the daemon process. Used by tests
/// that seed a precise registry status the startup reconcile sweep (Architecture
/// B) would otherwise settle -- e.g. `FNO_AGENTS_NO_STARTUP_RECONCILE=1` to keep
/// an artificially-seeded mid-flight source row intact for a promote-admission
/// assertion.
fn start_daemon_env(home: &AgentsHome, extra: &[(&str, &str)]) -> std::process::Child {
    let mut cmd = Command::new(DAEMON_BIN);
    cmd.env("FNO_AGENTS_HOME", home.root())
        .env("FNO_AGENTS_WORKER_BIN", WORKER_BIN)
        .env("FNO_AGENTS_IDLE_EXIT_SECS", "3600");
    for (k, v) in extra {
        cmd.env(k, v);
    }
    let child = cmd.spawn().expect("daemon spawns");
    wait_for(&home.supervisor_sock(), Duration::from_secs(10));
    child
}

fn wait_for(path: &Path, budget: Duration) {
    let start = Instant::now();
    while !path.exists() && start.elapsed() < budget {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(path.exists(), "path never appeared: {}", path.display());
}

fn line_count(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|body| body.lines().count())
        .unwrap_or(0)
}

fn wait_for_lines(path: &Path, minimum: usize, budget: Duration) -> usize {
    let start = Instant::now();
    loop {
        let count = line_count(path);
        if count >= minimum || start.elapsed() >= budget {
            return count;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn daemon_e2e_guard() -> std::sync::MutexGuard<'static, ()> {
    static GUARD: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    GUARD
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn seed_live_probe_rows(home: &AgentsHome, count: usize) {
    state::update_registry(&home.registry_json(), |registry| {
        for i in 0..count {
            let short_id = format!("{i:08x}");
            registry.entries.push(state::RegistryEntry {
                name: format!("slow-probe-{i:02}"),
                short_id: short_id.clone(),
                legacy_provider: "claude".into(),
                harness: Some("claude".into()),
                harness_session_id: Some(short_id.clone()),
                cwd: "/tmp".into(),
                project_root: String::new(),
                session_id: None,
                legacy_claude_short_id: Some(short_id),
                claude_session_uuid: None,
                messaging_socket_path: None,
                codex_session_id: None,
                gemini_session_id: None,
                mcp_channel_id: None,
                host_mode: None,
                cc_session_id: None,
                status: fno_agents::AgentStatus::Live,
                last_message_at: Some("2020-01-01T00:00:00Z".into()),
                created_at: "2020-01-01T00:00:00Z".into(),
                pid: Some(std::process::id()),
                pid_start_time: None,
                log_path: None,
                last_reconciled_at: None,
                inside_leg: None,
                exited_at: None,
                mux: None,
                screen_state: None,
                crown_level: None,
                crown_scope: None,
                crown_grantor: None,
                route_settings_path: None,
                fno_id: None,
                delivery_policy: None,
            });
        }
    })
    .unwrap();
}

struct SlowTruthShim {
    path: String,
    starts: PathBuf,
    finishes: PathBuf,
}

fn install_slow_truth_shim(home: &AgentsHome) -> SlowTruthShim {
    let shim_dir = home.root().join("bin");
    std::fs::create_dir_all(&shim_dir).unwrap();
    let starts = home.root().join("probe-starts");
    let finishes = home.root().join("probe-finishes");
    write_executable(
        &shim_dir.join("fno"),
        r#"#!/bin/sh
if [ "$1" != "agents" ] || [ "$2" != "truth" ]; then
  exit 0
fi
printf 'start\n' >> "$FNO_PROBE_STARTS"
sleep 1
case "$3" in
  *[13579bdf]) printf '{"state":"unknown"}\n'; status=13 ;;
  *) printf '{"state":"working"}\n'; status=0 ;;
esac
printf 'finish\n' >> "$FNO_PROBE_FINISHES"
exit "$status"
"#,
    );
    SlowTruthShim {
        path: format!(
            "{}:{}",
            shim_dir.display(),
            std::env::var("PATH").unwrap_or_default()
        ),
        starts,
        finishes,
    }
}

#[tokio::test]
async fn daemon_answers_before_probing_finishes() {
    let _test_guard = daemon_e2e_guard();
    let home = short_home();
    home.ensure_root().unwrap();
    seed_live_probe_rows(&home, 40);
    let shim = install_slow_truth_shim(&home);
    let starts_env = shim.starts.to_string_lossy().into_owned();
    let finishes_env = shim.finishes.to_string_lossy().into_owned();
    let mut daemon = start_daemon_env(
        &home,
        &[
            ("PATH", shim.path.as_str()),
            ("FNO_PROBE_STARTS", starts_env.as_str()),
            ("FNO_PROBE_FINISHES", finishes_env.as_str()),
            ("FNO_AGENTS_NO_STARTUP_RECONCILE", "1"),
            ("FNO_AGENTS_DEAD_ROW_GRACE_SECS", "0"),
        ],
    );

    assert!(
        wait_for_lines(&shim.starts, 1, Duration::from_secs(10)) >= 1,
        "the slow probe pass must have started before the request",
    );
    let request_started = Instant::now();
    let answer = tokio::time::timeout(
        Duration::from_secs(1),
        call(
            &home,
            &PathBuf::from(DAEMON_BIN),
            &Request::new(1, "agent.status", json!({})),
        ),
    )
    .await;
    let answer_elapsed = request_started.elapsed();
    let finishes_at_answer = line_count(&shim.finishes);
    let completed = wait_for_lines(&shim.finishes, 8, Duration::from_secs(12));

    unsafe {
        libc::kill(daemon.id() as libc::pid_t, libc::SIGTERM);
    }
    let _ = daemon.wait();
    std::fs::remove_dir_all(home.root()).ok();

    let response = answer
        .expect("daemon must answer within one second while probes are in flight")
        .expect("status request succeeds");
    assert!(!response.is_err(), "status failed: {:?}", response.error());
    assert!(answer_elapsed < Duration::from_secs(1));
    assert!(
        finishes_at_answer < 8,
        "the request answered only after the probe pass finished"
    );
    assert_eq!(completed, 8, "the bounded probe pass must later finish");
}

#[tokio::test]
async fn list_returns_partial_rows_before_the_client_deadline() {
    let _test_guard = daemon_e2e_guard();
    let home = short_home();
    home.ensure_root().unwrap();
    seed_live_probe_rows(&home, 40);
    let shim = install_slow_truth_shim(&home);
    let starts_env = shim.starts.to_string_lossy().into_owned();
    let finishes_env = shim.finishes.to_string_lossy().into_owned();
    let mut daemon = start_daemon_env(
        &home,
        &[
            ("PATH", shim.path.as_str()),
            ("FNO_PROBE_STARTS", starts_env.as_str()),
            ("FNO_PROBE_FINISHES", finishes_env.as_str()),
            ("FNO_AGENTS_NO_STARTUP_RECONCILE", "1"),
            ("FNO_AGENTS_DEAD_ROW_GRACE_SECS", "0"),
            ("FNO_AGENTS_PROBE_PASS_BUDGET_MS", "500"),
        ],
    );

    let started = Instant::now();
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        call(
            &home,
            &PathBuf::from(DAEMON_BIN),
            &Request::new(1, "agent.list", json!({"all": true})),
        ),
    )
    .await
    .expect("list must return before the client deadline")
    .expect("list request succeeds");
    let elapsed = started.elapsed();
    let agents = response.result().unwrap()["agents"]
        .as_array()
        .expect("list returns rows")
        .len();
    let probe_starts = line_count(&shim.starts);
    let events = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();

    unsafe {
        libc::kill(daemon.id() as libc::pid_t, libc::SIGTERM);
    }
    let _ = daemon.wait();
    std::fs::remove_dir_all(home.root()).ok();

    assert_eq!(agents, 40, "the bounded response keeps every stored row");
    assert!(elapsed < Duration::from_secs(2));
    assert!(
        probe_starts >= 1,
        "the bounded truth pass must actually run"
    );
    assert!(
        events.contains("list_truth_overrun"),
        "the partial-result path must emit its positive overrun marker"
    );
}

#[test]
fn sigterm_stops_daemon_during_slow_sweep() {
    let _test_guard = daemon_e2e_guard();
    let home = short_home();
    home.ensure_root().unwrap();
    seed_live_probe_rows(&home, 40);
    let shim = install_slow_truth_shim(&home);
    let starts_env = shim.starts.to_string_lossy().into_owned();
    let finishes_env = shim.finishes.to_string_lossy().into_owned();
    let mut daemon = start_daemon_env(
        &home,
        &[
            ("PATH", shim.path.as_str()),
            ("FNO_PROBE_STARTS", starts_env.as_str()),
            ("FNO_PROBE_FINISHES", finishes_env.as_str()),
            ("FNO_AGENTS_NO_STARTUP_RECONCILE", "1"),
            ("FNO_AGENTS_DEAD_ROW_GRACE_SECS", "0"),
        ],
    );
    assert!(
        wait_for_lines(&shim.starts, 1, Duration::from_secs(10)) >= 1,
        "the slow probe pass must be in flight before SIGTERM",
    );

    unsafe {
        libc::kill(daemon.id() as libc::pid_t, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let exited = loop {
        match daemon.try_wait().expect("daemon remains waitable") {
            Some(status) => break status.success(),
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            None => {
                unsafe {
                    libc::kill(daemon.id() as libc::pid_t, libc::SIGKILL);
                }
                let _ = daemon.wait();
                break false;
            }
        }
    };
    let finishes_at_exit = line_count(&shim.finishes);
    std::fs::remove_dir_all(home.root()).ok();

    assert!(
        exited,
        "SIGTERM alone must stop the daemon within five seconds"
    );
    assert!(
        finishes_at_exit < 8,
        "shutdown waited for the entire blocking probe pass"
    );
}

#[test]
fn wedged_daemon_clients_fail_fast_no_orphans() {
    let _test_guard = daemon_e2e_guard();
    let home = short_home();
    home.ensure_root().unwrap();
    let fake_daemon = home.root().join("wedged-daemon");
    write_executable(
        &fake_daemon,
        r#"#!/usr/bin/env python3
import os
import socket
import time

home = os.environ["FNO_AGENTS_HOME"]
with open(os.path.join(home, "spawn-count"), "a", encoding="utf-8") as stream:
    stream.write("spawned\n")
with open(os.path.join(home, "daemon.pid"), "w", encoding="utf-8") as stream:
    stream.write(str(os.getpid()))
sock_path = os.path.join(home, "supervisor.sock")
try:
    os.unlink(sock_path)
except FileNotFoundError:
    pass
listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
listener.bind(sock_path)
listener.listen(128)
while True:
    time.sleep(1)
"#,
    );

    let started = Instant::now();
    let spawn_client = || {
        Command::new(CLIENT_BIN)
            .args(["rm", "no-such-worker-xyz"])
            .env("FNO_AGENTS_HOME", home.root())
            .env("FNO_AGENTS_DAEMON_BIN", &fake_daemon)
            .env("FNO_AGENTS_START_TIMEOUT_MS", "500")
            .env("FNO_AGENTS_REQUEST_TIMEOUT_MS", "200")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("client spawns")
    };
    let mut clients = vec![spawn_client()];
    wait_for(&home.supervisor_sock(), Duration::from_secs(1));
    for _ in 1..20 {
        clients.push(spawn_client());
    }
    let statuses: Vec<_> = clients
        .iter_mut()
        .map(|client| client.wait().expect("client exits"))
        .collect();
    let elapsed = started.elapsed();

    assert!(
        statuses.iter().all(|status| !status.success()),
        "every client must fail against a listener that never answers"
    );
    assert!(
        statuses.iter().any(|status| status.code() == Some(124)),
        "a bounded request timeout must use the dedicated CLI exit code"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "twenty clients took {elapsed:?} instead of failing within their bounds"
    );
    assert_eq!(
        line_count(&home.root().join("spawn-count")),
        1,
        "the client startup lock must elect exactly one daemon child"
    );
    let pid: u32 = std::fs::read_to_string(home.root().join("daemon.pid"))
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        !pid_alive(pid),
        "the elected daemon child must be reaped after it fails to answer"
    );
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn final_answer_check_keeps_a_daemon_that_binds_at_the_deadline() {
    let _test_guard = daemon_e2e_guard();
    let home = short_home();
    home.ensure_root().unwrap();
    let delayed_daemon = home.root().join("delayed-daemon");
    write_executable(
        &delayed_daemon,
        &format!(
            "#!/bin/sh\nsleep 0.15\nexec \"{}\"\n",
            DAEMON_BIN.replace('"', "\\\"")
        ),
    );

    let started = Instant::now();
    let first_status = Command::new(CLIENT_BIN)
        .args(["rm", "no-such-worker-xyz"])
        .env("FNO_AGENTS_HOME", home.root())
        .env("FNO_AGENTS_DAEMON_BIN", &delayed_daemon)
        .env("FNO_AGENTS_START_TIMEOUT_MS", "100")
        .env("FNO_AGENTS_REQUEST_TIMEOUT_MS", "500")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("client runs");
    assert!(
        !first_status.success(),
        "the missing worker should still be reported as absent"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the deadline-race request exceeded its configured bounds"
    );

    let status = Command::new(CLIENT_BIN)
        .arg("status")
        .env("FNO_AGENTS_HOME", home.root())
        .env("FNO_AGENTS_REQUEST_TIMEOUT_MS", "500")
        .output()
        .expect("status client runs");
    assert!(
        status.status.success(),
        "the late-binding daemon was killed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let body: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    let pid = body["daemon"]["pid"].as_u64().unwrap() as u32;
    assert!(pid_alive(pid));
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while home.supervisor_sock().exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        !home.supervisor_sock().exists(),
        "SIGTERM must remove the late-binding daemon's socket"
    );
    std::fs::remove_dir_all(home.root()).ok();
}

/// AC1-HP (Architecture B, plan ab-70faa65b): a cold daemon start runs ONE
/// bounded reconcile sweep concurrently with serving. A stale `ask` row recorded
/// `live` at creation (its one-shot process long gone) settles to `exited` -- even
/// though its provider session id makes it "resumable" -- and surfaces that
/// resumability via `session_id`. The startup sweep is what flips the stored row:
/// no explicit `reconcile` RPC is issued here.
#[tokio::test]
async fn cold_start_reconciles_stale_ask_row_to_exited() {
    let _test_guard = daemon_e2e_guard();
    let home = short_home();
    home.ensure_root().unwrap();
    let daemon_bin = PathBuf::from(DAEMON_BIN);

    // Seed a stale ask row (empty short_id + no pid = one-shot ask) while the
    // daemon is DOWN, status recorded `live` and never reconciled.
    state::update_registry(&home.registry_json(), |r| {
        r.entries.push(state::RegistryEntry {
            name: "stale-ask".into(),
            short_id: String::new(),
            legacy_provider: "codex".into(),
            harness: None,
            harness_session_id: None,
            cwd: "/tmp".into(),
            project_root: String::new(),
            session_id: None,
            legacy_claude_short_id: None,
            claude_session_uuid: None,
            messaging_socket_path: None,
            codex_session_id: Some("resume-uuid-xyz".into()),
            gemini_session_id: None,
            mcp_channel_id: None,
            host_mode: None,
            cc_session_id: None,
            status: fno_agents::AgentStatus::Live,
            last_message_at: None,
            created_at: "2026-05-29T00:00:00Z".into(),
            pid: None,
            pid_start_time: None,
            log_path: None,
            last_reconciled_at: None,
            inside_leg: None,
            exited_at: None,
            mux: None,
            screen_state: None,
            crown_level: None,
            crown_scope: None,
            crown_grantor: None,
            route_settings_path: None,
            fno_id: None,
            delivery_policy: None,
        });
    })
    .unwrap();

    let mut daemon = start_daemon(&home);

    // Serving and reconciliation are concurrent. The list truth pass must render
    // current liveness without waiting for the startup sweep to commit.
    let resp = call(
        &home,
        &daemon_bin,
        &Request::new(1, "agent.list", json!({"all": true})),
    )
    .await
    .expect("list call");
    assert!(!resp.is_err(), "list failed: {:?}", resp.error());
    let agents = resp.result().unwrap()["agents"].as_array().unwrap().clone();
    let row = agents
        .iter()
        .find(|a| a["name"] == "stale-ask")
        .expect("stale-ask row present");
    assert_eq!(
        row["status"], "unknown",
        "rendered liveness must not inherit the stored lifecycle status"
    );
    // Resumability (session_id) is independent of liveness (status) -- AC3-EDGE.
    assert_eq!(row["session_id"], "resume-uuid-xyz");
    // AC4-EDGE: a one-shot ask has no managed process, so pid is null in --json.
    assert!(row["pid"].is_null(), "ask row must have null pid: {row}");

    // Wait on the sweep's positive completion marker before asserting its
    // registry write. An absence-only check would pass if the sweep never ran.
    let deadline = Instant::now() + Duration::from_secs(10);
    let events = loop {
        let events = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
        if events.contains("startup_reconcile_done") {
            break events;
        }
        assert!(
            Instant::now() < deadline,
            "startup_reconcile_done event not emitted"
        );
        std::thread::sleep(Duration::from_millis(25));
    };

    // The startup sweep wrote the registry: status exited + CHECKED stamped.
    let reg = state::load_registry(&home.registry_json()).unwrap();
    let entry = reg.find("stale-ask").unwrap();
    assert_eq!(entry.status, fno_agents::AgentStatus::Exited);
    assert!(
        entry.last_reconciled_at.is_some(),
        "startup sweep must stamp last_reconciled_at (CHECKED freshens, AC1-UI)"
    );
    assert_eq!(entry.pid, None, "ask row never carries a pid");

    assert!(
        events.contains("startup_reconcile_done"),
        "startup_reconcile_done event not emitted"
    );

    unsafe {
        libc::kill(daemon.id() as libc::pid_t, libc::SIGTERM);
    }
    let _ = daemon.wait();
    std::fs::remove_dir_all(home.root()).ok();
}

/// AC1-FR (Architecture B, plan ab-70faa65b): a failing startup reconcile sweep
/// degrades to serving last-recorded status -- the daemon still comes up and
/// serves `list`, an event records the failure, and the seeded row keeps its
/// recorded status (the sweep never applied) -- rather than aborting. The
/// failure is injected via the FNO_AGENTS_FAIL_STARTUP_RECONCILE test seam.
#[tokio::test]
async fn startup_reconcile_failure_degrades_to_serving() {
    let _test_guard = daemon_e2e_guard();
    let home = short_home();
    home.ensure_root().unwrap();
    let daemon_bin = PathBuf::from(DAEMON_BIN);
    // A row recorded `live`; with the sweep forced to fail it must NOT settle.
    seed_codex_source(
        &home,
        "kept-live",
        "uuid-fr-1",
        fno_agents::AgentStatus::Live,
    );
    let mut daemon = start_daemon_env(&home, &[("FNO_AGENTS_FAIL_STARTUP_RECONCILE", "1")]);

    // The daemon still serves despite the failed startup sweep (did not abort).
    let resp = call(
        &home,
        &daemon_bin,
        &Request::new(1, "agent.list", json!({"all": true})),
    )
    .await
    .expect("list served despite startup-sweep failure");
    assert!(!resp.is_err(), "list failed: {:?}", resp.error());
    let agents = resp.result().unwrap()["agents"].as_array().unwrap().clone();
    let row = agents
        .iter()
        .find(|a| a["name"] == "kept-live")
        .expect("row present");
    assert_eq!(
        row["status"], "unknown",
        "rendered liveness must not inherit the stored lifecycle status"
    );
    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert_eq!(
        reg.find("kept-live").unwrap().status,
        fno_agents::AgentStatus::Live,
        "failed sweep must not mutate stored status"
    );

    let events = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
    assert!(
        events.contains("startup_reconcile_failed"),
        "a failed startup sweep must emit startup_reconcile_failed"
    );

    unsafe {
        libc::kill(daemon.id() as libc::pid_t, libc::SIGTERM);
    }
    let _ = daemon.wait();
    std::fs::remove_dir_all(home.root()).ok();
}

/// status (Wave 5, US6.10, AC10-ERR): with no daemon running, the `fno-agents
/// status` client exits 13 and does NOT lazy-start a daemon.
#[tokio::test]
async fn status_client_exits_13_when_daemon_down() {
    let _test_guard = daemon_e2e_guard();
    const CLIENT_BIN: &str = env!("CARGO_BIN_EXE_fno-agents");
    let home = short_home();
    home.ensure_root().unwrap();

    let out = Command::new(CLIENT_BIN)
        .arg("status")
        .env("FNO_AGENTS_HOME", home.root())
        .output()
        .expect("client runs");
    assert_eq!(
        out.status.code(),
        Some(13),
        "status with no daemon must exit 13; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    // It must NOT have lazy-started a daemon (no socket left behind).
    assert!(
        !home.supervisor_sock().exists(),
        "status must not start a daemon when one is down"
    );
    std::fs::remove_dir_all(home.root()).ok();
}

/// Seed a settled exec source row so a promote can resolve it. The daemon reads
/// the registry file fresh on each handle_spawn, so a directly-seeded row is
/// visible to admit_promote. short_id empty => Python-style/non-PTY source.
fn seed_codex_source(home: &AgentsHome, name: &str, uuid: &str, status: fno_agents::AgentStatus) {
    state::update_registry(&home.registry_json(), |r| {
        r.entries.push(fno_agents::state::RegistryEntry {
            name: name.into(),
            short_id: String::new(),
            legacy_provider: "codex".into(),
            harness: None,
            harness_session_id: None,
            cwd: "/tmp".into(),
            project_root: String::new(),
            session_id: None,
            legacy_claude_short_id: None,
            claude_session_uuid: None,
            messaging_socket_path: None,
            codex_session_id: Some(uuid.into()),
            gemini_session_id: None,
            mcp_channel_id: None,
            host_mode: None,
            cc_session_id: None,
            status,
            last_message_at: None,
            created_at: "2026-05-29T00:00:00Z".into(),
            pid: None,
            pid_start_time: None,
            log_path: None,
            last_reconciled_at: None,
            inside_leg: None,
            exited_at: None,
            mux: None,
            screen_state: None,
            crown_level: None,
            crown_scope: None,
            crown_grantor: None,
            route_settings_path: None,
            fno_id: None,
            delivery_policy: None,
        });
    })
    .unwrap();
}

/// Seed a pane-hosted row: the shape whose identity keys the list projection
/// used to drop. Holds the mux ref INSTEAD of a transport key (mux XOR worker
/// XOR bg), so `short_id` is empty and `harness_session_id` is the only id.
fn seed_pane_row(home: &AgentsHome, name: &str) {
    state::update_registry(&home.registry_json(), |r| {
        r.entries.push(fno_agents::state::RegistryEntry {
            name: name.into(),
            short_id: String::new(),
            legacy_provider: "claude".into(),
            harness: Some("claude".into()),
            harness_session_id: Some("e6f78b98-e594-47ed-ad81-84f8a78b8bb7".into()),
            cwd: "/tmp".into(),
            project_root: String::new(),
            session_id: None,
            legacy_claude_short_id: None,
            claude_session_uuid: None,
            messaging_socket_path: None,
            codex_session_id: None,
            gemini_session_id: None,
            mcp_channel_id: None,
            host_mode: None,
            cc_session_id: None,
            status: fno_agents::AgentStatus::Live,
            last_message_at: None,
            created_at: "2026-07-30T00:00:00Z".into(),
            pid: None,
            pid_start_time: None,
            log_path: None,
            last_reconciled_at: None,
            inside_leg: None,
            exited_at: None,
            mux: Some(fno_agents::state::MuxRef {
                session: "main".into(),
                pane_id: 10,
            }),
            screen_state: None,
            crown_level: None,
            crown_scope: None,
            crown_grantor: None,
            route_settings_path: None,
            fno_id: None,
            delivery_policy: None,
        });
    })
    .unwrap();
}

fn write_executable(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).unwrap();
}

#[tokio::test]
async fn rm_reaps_registry_claude_and_mux_surfaces_in_one_call() {
    let _test_guard = daemon_e2e_guard();
    let home = short_home();
    home.ensure_root().unwrap();
    seed_pane_row(&home, "three-surface-worker");
    state::update_registry(&home.registry_json(), |registry| {
        let row = registry.find_mut("three-surface-worker").unwrap();
        row.status = fno_agents::AgentStatus::Exited;
        row.exited_at = None;
    })
    .unwrap();

    let shim_dir = home.root().join("shims");
    std::fs::create_dir_all(&shim_dir).unwrap();
    let claude_state = home.root().join("claude-row");
    let mux_state = home.root().join("mux-pane");
    std::fs::write(&claude_state, "e6f78b98\n").unwrap();
    std::fs::write(&mux_state, "main:10\n").unwrap();
    write_executable(
        &shim_dir.join("claude"),
        r#"#!/bin/sh
if [ "$1" = "agents" ]; then
  if [ -f "$CLAUDE_STATE" ]; then
    printf '[{"kind":"background","id":"e6f78b98","state":"stopped"}]\n'
  else
    printf '[]\n'
  fi
  exit 0
fi
if [ "$1" = "rm" ] && [ "$2" = "e6f78b98" ]; then
  /bin/rm -f "$CLAUDE_STATE"
  exit 0
fi
exit 2
"#,
    );
    write_executable(
        &shim_dir.join("fno"),
        r#"#!/bin/sh
if [ "$1" = "mux" ] && [ "$2" = "pane" ] && [ "$3" = "kill" ] && \
   [ "$4" = "--session" ] && [ "$5" = "main" ] && [ "$6" = "10" ]; then
  /bin/rm -f "$MUX_STATE"
  exit 0
fi
if [ "$1" = "mux" ] && [ "$2" = "pane" ] && [ "$3" = "ls" ]; then
  if [ -f "$MUX_STATE" ]; then
    printf '[{"session":"main","pane_id":10}]\n'
  else
    printf '[]\n'
  fi
  exit 0
fi
exit 2
"#,
    );

    let path = format!(
        "{}:{}",
        shim_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let claude_state_env = claude_state.to_string_lossy().into_owned();
    let mux_state_env = mux_state.to_string_lossy().into_owned();
    let mut daemon = start_daemon_env(
        &home,
        &[
            ("PATH", &path),
            ("CLAUDE_STATE", &claude_state_env),
            ("MUX_STATE", &mux_state_env),
            ("FNO_AGENTS_NO_STARTUP_RECONCILE", "1"),
        ],
    );

    assert_eq!(
        state::load_registry(&home.registry_json())
            .unwrap()
            .entries
            .len(),
        1
    );
    let pre_claude = Command::new(shim_dir.join("claude"))
        .args(["agents", "--json", "--all"])
        .env("CLAUDE_STATE", &claude_state_env)
        .output()
        .unwrap();
    let pre_mux = Command::new(shim_dir.join("fno"))
        .args(["mux", "pane", "ls"])
        .env("MUX_STATE", &mux_state_env)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&pre_claude.stdout).contains("e6f78b98"));
    assert!(String::from_utf8_lossy(&pre_mux.stdout).contains("pane_id"));

    let out = Command::new(CLIENT_BIN)
        .args(["rm", "three-surface-worker"])
        .env("FNO_AGENTS_HOME", home.root())
        .output()
        .expect("rm client runs");
    assert!(
        out.status.success(),
        "rm failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("fno + claude + mux"),
        "receipt: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    let post_registry = state::load_registry(&home.registry_json()).unwrap();
    let post_claude = Command::new(shim_dir.join("claude"))
        .args(["agents", "--json", "--all"])
        .env("CLAUDE_STATE", &claude_state_env)
        .output()
        .unwrap();
    let post_mux = Command::new(shim_dir.join("fno"))
        .args(["mux", "pane", "ls"])
        .env("MUX_STATE", &mux_state_env)
        .output()
        .unwrap();
    assert_eq!(post_registry.entries.len(), 0, "registry row survived");
    assert_eq!(String::from_utf8_lossy(&post_claude.stdout).trim(), "[]");
    assert_eq!(String::from_utf8_lossy(&post_mux.stdout).trim(), "[]");

    unsafe {
        libc::kill(daemon.id() as libc::pid_t, libc::SIGTERM);
    }
    let _ = daemon.wait();
    std::fs::remove_dir_all(home.root()).ok();
}

// ---------------------------------------------------------------------------
// Daemon binary-version drift restart (ab-1891cdff): US2 (restart swaps the
// daemon), US3 (PTY workers survive -- Outcome B), US1/US4 (drift warned on
// list, stderr-only so --json stdout stays clean).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn restart_when_down_starts_fresh() {
    let _test_guard = daemon_e2e_guard();
    // AC2-EDGE: no daemon running -> restart starts a fresh one and reports it,
    // with no error and old_pid == None.
    let home = short_home();
    home.ensure_root().unwrap();
    let daemon_bin = PathBuf::from(DAEMON_BIN);
    std::env::set_var("FNO_AGENTS_WORKER_BIN", WORKER_BIN);

    let outcome = fno_agents::client::restart_daemon(&home, &daemon_bin)
        .await
        .expect("restart-when-down succeeds");
    assert_eq!(outcome.old_pid, None, "nothing was running");
    assert!(pid_alive(outcome.new_pid), "fresh daemon is alive");

    unsafe {
        libc::kill(outcome.new_pid as libc::pid_t, libc::SIGTERM);
    }
    std::thread::sleep(Duration::from_millis(200));
    std::fs::remove_dir_all(home.root()).ok();
}

#[tokio::test]
async fn drift_warned_on_list_stderr_only() {
    let _test_guard = daemon_e2e_guard();
    // AC1-HP + AC4-HP: a daemon whose on-disk binary was replaced after startup
    // makes `list --json` emit a drift warning on STDERR while STDOUT stays clean
    // JSON.
    let home = short_home();
    home.ensure_root().unwrap();

    // A private copy of the daemon binary we can replace out from under the
    // running process (the running process keeps the old inode).
    let dcopy = PathBuf::from(format!(
        "/tmp/fnodcopy_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::copy(DAEMON_BIN, &dcopy).expect("copy daemon bin");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dcopy, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    // Start the daemon FROM the copy so its current_exe() == dcopy; it records the
    // copy's fingerprint at startup. Retry on ETXTBSY: a binary freshly written by
    // fs::copy can briefly refuse to exec ("Text file busy", code 26) while the
    // kernel still holds a write reference, racing this copy-then-spawn on a loaded
    // CI runner. The window is milliseconds; bound the retry so a real failure fails.
    let spawn_daemon = || {
        Command::new(&dcopy)
            .env("FNO_AGENTS_HOME", home.root())
            .env("FNO_AGENTS_WORKER_BIN", WORKER_BIN)
            .env("FNO_AGENTS_IDLE_EXIT_SECS", "3600")
            .spawn()
    };
    let mut daemon = {
        let mut attempt = 0;
        loop {
            match spawn_daemon() {
                Ok(child) => break child,
                Err(e) if e.raw_os_error() == Some(26) && attempt < 50 => {
                    attempt += 1;
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => panic!("daemon spawns from copy: {e:?}"),
            }
        }
    };
    wait_for(&home.supervisor_sock(), Duration::from_secs(10));

    // One real row, so this is also the only coverage of a row projection
    // travelling the whole client -> socket -> daemon -> stdout path. The unit
    // test calls the projection directly and so cannot see the client seam;
    // with an empty registry this test proved only that stdout parses.
    //
    // Seeded AFTER startup on purpose: the daemon re-reads the registry on every
    // list, so the row is still visible, and this leaves the cold-start reconcile
    // sweep walking an empty registry exactly as it did before. Seeding first
    // hands the sweep a pane row to probe on a test whose subject is binary
    // drift, which is latency and failure surface this test should not own.
    seed_pane_row(&home, "worker-pane-e2e");

    // A served status RPC only returns once the daemon is in its accept loop,
    // which is AFTER it records its exe fingerprint at startup. Gating the
    // replace on this proves the daemon fingerprinted the ORIGINAL copy, closing
    // a parallel-run race where a slow startup recorded the post-replace file and
    // read Fresh. The original-size assertion makes that intent explicit.
    let original_size = std::fs::metadata(&dcopy).unwrap().len();
    let status =
        fno_agents::client::call_if_running(&home, &Request::new(7, "agent.status", json!({})))
            .await
            .expect("status before replace");
    assert_eq!(
        status.result().unwrap()["daemon"]["exe_size"].as_u64(),
        Some(original_size),
        "daemon recorded the original copy's fingerprint before replace"
    );

    // Replace the on-disk copy with a different-sized file at the SAME path (the
    // running daemon still holds the old inode) -> content drift.
    std::fs::remove_file(&dcopy).unwrap();
    std::fs::write(&dcopy, b"stale-stub").unwrap();

    // Run the real client `list --json` against this daemon, pointing
    // resolve_daemon_bin() at the (now-replaced) copy.
    let out = Command::new(CLIENT_BIN)
        .args(["list", "--json"])
        .env("FNO_AGENTS_HOME", home.root())
        .env("FNO_AGENTS_DAEMON_BIN", &dcopy)
        .env("FNO_AGENTS_WORKER_BIN", WORKER_BIN)
        .output()
        .expect("client list runs");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(out.status.success(), "client list exited {:?}", out.status);

    // AC4-HP: stdout is valid JSON with no warning text.
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("list --json stdout is valid JSON");

    // The row survives the whole real path, not just the in-process projection:
    // the client splices daemon rows verbatim today, and nothing else fails if a
    // future change starts projecting a subset there. Assert the WHOLE key set
    // against the same contract the daemon test uses; a few hand-picked keys
    // would leave every other key unguarded at this seam.
    const CONTRACT: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../schemas/agents-list-row.json"
    ));
    let contract: serde_json::Value =
        serde_json::from_str(CONTRACT).expect("contract is valid JSON");
    let mut expected: std::collections::BTreeSet<String> = contract["required"]
        .as_array()
        .expect("required is an array")
        .iter()
        .map(|k| k.as_str().unwrap().to_string())
        .collect();
    expected.extend(
        contract["rust_only"]["keys"]
            .as_array()
            .expect("rust_only.keys is an array")
            .iter()
            .map(|k| k.as_str().unwrap().to_string()),
    );

    let row = &parsed["agents"][0];
    let actual: std::collections::BTreeSet<String> = row
        .as_object()
        .expect("agents[0] is an object (a zero-row list fails here)")
        .keys()
        .cloned()
        .collect();
    assert_eq!(actual, expected, "row key set drifted at the client seam");

    assert_eq!(row["name"], "worker-pane-e2e");
    assert_eq!(row["harness"], "claude");
    assert_eq!(
        row["harness_session_id"],
        "e6f78b98-e594-47ed-ad81-84f8a78b8bb7"
    );
    assert_eq!(row["mux"]["session"], "main");
    assert_eq!(row["mux"]["pane_id"], 10);
    assert!(
        !stdout.contains("restart") && !stdout.contains("older build"),
        "warning leaked into stdout: {stdout}"
    );
    // AC1-HP: the warning is on stderr and names the restart verb.
    assert!(
        stderr.contains("fno agents restart") && stderr.contains("build"),
        "expected drift warning on stderr, got: {stderr}"
    );

    unsafe {
        libc::kill(daemon.id() as libc::pid_t, libc::SIGTERM);
    }
    let _ = daemon.wait();
    std::fs::remove_file(&dcopy).ok();
    std::fs::remove_dir_all(home.root()).ok();
}

// ---------------------------------------------------------------------------
// x-4c87: a nonempty registry must never decode to an empty roster silently.
// The live outage (2026-08-16): a stale daemon whose v11 reader refused the
// v14 store swallowed the read failure into `Registry::default()` and answered
// `count: 0` beside a healthy `discovered_count`, while mail lookup printed
// `agent '<name>' not found` for live workers. The fixtures below are the
// sanitized same-schema equivalent: real worker row shapes, one row carrying a
// value the typed model cannot represent (an unknown status variant).
// ---------------------------------------------------------------------------

/// A sanitized 3-row registry (2 valid, 1 the typed reader cannot represent),
/// written RAW so the typed writer can never heal or reformat it first.
fn write_divergent_registry(home: &AgentsHome) {
    let row = |name: &str, status: &str| {
        format!(
            r#"{{"name":"{name}","cwd":"/tmp/proj","harness":"claude","harness_session_id":"11111111-2222-3333-4444-555555555555","status":"{status}","created_at":"2026-08-16T00:00:00Z"}}"#
        )
    };
    let body = format!(
        r#"{{"schema_version":14,"agents":[{},{},{}]}}"#,
        row("worker-alpha", "live"),
        row("worker-beta", "hibernating"),
        row("worker-gamma", "live")
    );
    std::fs::write(home.registry_json(), body).expect("seed divergent registry");
}

/// A valid 2-row registry at the current schema.
fn write_valid_registry(home: &AgentsHome) {
    let row = |name: &str| {
        format!(
            r#"{{"name":"{name}","cwd":"/tmp/proj","harness":"claude","harness_session_id":"11111111-2222-3333-4444-555555555555","status":"live","created_at":"2026-08-16T00:00:00Z"}}"#
        )
    };
    let body = format!(
        r#"{{"schema_version":14,"agents":[{},{}]}}"#,
        row("worker-alpha"),
        row("worker-gamma")
    );
    std::fs::write(home.registry_json(), body).expect("seed valid registry");
}

/// AC5-HP: a divergent registry (3 raw rows, typed decode fails) must refuse
/// daemon startup: no serving socket is published, the process exits nonzero,
/// and stderr names the registry path plus both row counts. The
/// `unwrap_or_default()` this replaced let the daemon come up believing zero
/// agents and serve that false zero to every caller.
#[tokio::test]
async fn registry_startup_refuses_a_divergent_nonempty_registry() {
    let _test_guard = daemon_e2e_guard();
    let home = short_home();
    home.ensure_root().unwrap();
    write_divergent_registry(&home);

    let mut child = Command::new(DAEMON_BIN)
        .env("FNO_AGENTS_HOME", home.root())
        .env("FNO_AGENTS_WORKER_BIN", WORKER_BIN)
        .env("FNO_AGENTS_IDLE_EXIT_SECS", "3600")
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("daemon spawns");

    // Bounded wait for exit: the refusal happens before bind, so the process
    // should die in well under a second. A daemon that STARTS serving instead
    // hangs until idle-exit, and the deadline turns that into a failure here
    // rather than a stuck test.
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait().expect("daemon is waitable") {
            Some(status) => break status,
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            None => {
                unsafe {
                    libc::kill(child.id() as libc::pid_t, libc::SIGKILL);
                }
                let _ = child.wait();
                panic!("daemon served instead of refusing a divergent registry");
            }
        }
    };
    assert!(
        !status.success(),
        "daemon must exit nonzero on a divergent registry"
    );
    assert!(
        !home.supervisor_sock().exists(),
        "no serving socket may be published over a divergent registry"
    );
    let mut stderr_buf = String::new();
    if let Some(mut s) = child.stderr.take() {
        use std::io::Read;
        let _ = s.read_to_string(&mut stderr_buf);
    }
    let stderr = stderr_buf;
    let reg_path = home.registry_json();
    assert!(
        stderr.contains(reg_path.to_str().unwrap()),
        "stderr names the registry path: {stderr}"
    );
    assert!(
        stderr.contains("raw_rows=3") && stderr.contains("decoded_rows=0"),
        "stderr carries both row counts: {stderr}"
    );
    assert!(stderr.contains("inspect"), "points at the file: {stderr}");
    for banned in ["force", "skip", "ignore", "bypass", "no-verify"] {
        assert!(
            !stderr.to_lowercase().contains(banned),
            "diagnostic must not name an override remedy ({banned}): {stderr}"
        );
    }
    std::fs::remove_dir_all(home.root()).ok();
}

/// AC6-ERR + AC9-CON: with the daemon up and the registry THEN breaking, list
/// and list --all must exit nonzero with `registry read failed` and must not
/// print a successful payload that puts `count: 0` beside a positive
/// discovered count.
#[tokio::test]
async fn registry_list_refuses_over_a_broken_registered_lane() {
    let _test_guard = daemon_e2e_guard();
    let home = short_home();
    home.ensure_root().unwrap();
    write_valid_registry(&home);
    let mut daemon = start_daemon(&home);

    // Healthy first: the registered lane really carried the 2 rows.
    let out = Command::new(CLIENT_BIN)
        .args(["list", "--all", "--json"])
        .env("FNO_AGENTS_HOME", home.root())
        .output()
        .expect("client list runs");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "pre-break list failed: {stdout}");
    assert!(stdout.contains("worker-alpha"), "row missing: {stdout}");

    // Break the registered lane out from under the running daemon.
    write_divergent_registry(&home);

    for args in [
        ["list", "--all", "--json"].as_slice(),
        ["list", "--json"].as_slice(),
    ] {
        let out = Command::new(CLIENT_BIN)
            .args(args)
            .env("FNO_AGENTS_HOME", home.root())
            .output()
            .expect("client list runs");
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        assert!(
            !out.status.success(),
            "{args:?} must exit nonzero over a broken registry, got success: {stdout}"
        );
        assert!(
            stderr.contains("registry read failed"),
            "stderr must name the failed read: {stderr}"
        );
        assert!(
            stderr.contains("raw_rows=3") && stderr.contains("decoded_rows=0"),
            "stderr must carry both counts: {stderr}"
        );
        assert!(
            !stdout.contains("\"count\""),
            "no successful payload may publish a count over a broken lane: {stdout}"
        );
    }

    unsafe {
        libc::kill(daemon.id() as libc::pid_t, libc::SIGTERM);
    }
    let _ = daemon.wait();
    std::fs::remove_dir_all(home.root()).ok();
}

/// AC7-ERR + AC8-HP: the mail-delivery name lookup (agent.switchboard_v2,
/// the handler behind `fno mail send`) reports an unreadable registry as an
/// internal read failure -- never `AgentNotFound` for a name its raw rows
/// demonstrably carry. A readable registry that genuinely lacks the name
/// still returns `AgentNotFound`.
#[tokio::test]
async fn registry_lookup_distinguishes_unreadable_from_absent() {
    let _test_guard = daemon_e2e_guard();
    let home = short_home();
    home.ensure_root().unwrap();
    let daemon_bin = PathBuf::from(DAEMON_BIN);

    // Unreadable registry: the recipient IS present in the raw rows. The
    // daemon starts on a HEALTHY registry (a divergent one refuses startup,
    // see registry_startup_refuses_a_divergent_nonempty_registry) and the
    // registry then breaks under it -- the live shape: a newer writer lands
    // a row the running reader cannot represent.
    write_valid_registry(&home);
    let mut daemon = start_daemon(&home);
    // Barrier: wait out the startup reconcile sweep before breaking the
    // registry. The sweep's read-modify-write of a registry it read as VALID
    // would otherwise overwrite the divergent fixture: its read-forward drops
    // the unrepresentable row and writes the rest back, healing the file so
    // the lookup reads a valid registry and the test fails with a WRONG
    // VERDICT instead of a wait problem. So the barrier waits on the sweep's
    // OWN completion marker, and a marker that never arrives FAILS the test
    // instead of silently proceeding - a silent timeout here converts "did
    // not wait" into "lookup succeeded".
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let events = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
        if events.contains("startup_reconcile_done") || events.contains("startup_reconcile_failed")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "startup reconcile never finished; breaking the registry now would race the sweep"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    write_divergent_registry(&home);

    let resp = call(
        &home,
        &daemon_bin,
        &Request::new(
            11,
            "agent.switchboard_v2",
            json!({
                "to": "worker-alpha",
                "from": "worker-omega",
                "body": "ping",
                "recipient_identity": {},
                "mirror": false,
            }),
        ),
    )
    .await
    .expect("switchboard call");
    let err = resp.error().expect("broken registry must error the lookup");
    assert!(
        err.message.contains("registry read failed"),
        "must name the failed read, got: {}",
        err.message
    );
    assert!(
        err.message.contains("raw_rows=3") && err.message.contains("decoded_rows=0"),
        "must carry both counts: {}",
        err.message
    );

    unsafe {
        libc::kill(daemon.id() as libc::pid_t, libc::SIGTERM);
    }
    let _ = daemon.wait();
    std::fs::remove_dir_all(home.root()).ok();

    // Control: a readable registry without the name keeps the absent contract.
    let home = short_home();
    home.ensure_root().unwrap();
    write_valid_registry(&home);
    let mut daemon = start_daemon(&home);

    let resp = call(
        &home,
        &daemon_bin,
        &Request::new(
            12,
            "agent.switchboard_v2",
            json!({
                "to": "ghost-worker",
                "from": "worker-omega",
                "body": "ping",
                "recipient_identity": {},
                "mirror": false,
            }),
        ),
    )
    .await
    .expect("switchboard call");
    let err = resp.error().expect("absent name must still error");
    assert!(
        err.message.contains("'ghost-worker' not found"),
        "a true miss keeps AgentNotFound wording: {}",
        err.message
    );
    assert!(
        !err.message.contains("registry read failed"),
        "a readable miss is not a read failure: {}",
        err.message
    );

    unsafe {
        libc::kill(daemon.id() as libc::pid_t, libc::SIGTERM);
    }
    let _ = daemon.wait();
    std::fs::remove_dir_all(home.root()).ok();
}

/// AC4-EDGE control: a genuinely empty registry (a real empty array) starts
/// the daemon and serves a legitimate count of zero.
#[tokio::test]
async fn registry_true_empty_registry_still_serves_zero() {
    let _test_guard = daemon_e2e_guard();
    let home = short_home();
    home.ensure_root().unwrap();
    std::fs::write(home.registry_json(), r#"{"schema_version":14,"agents":[]}"#).unwrap();
    let mut daemon = start_daemon(&home);

    let out = Command::new(CLIENT_BIN)
        .args(["list", "--all", "--json"])
        .env("FNO_AGENTS_HOME", home.root())
        .output()
        .expect("client list runs");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success(),
        "a true empty is a valid zero-agent state: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("\"count\": 0"),
        "count 0 published: {stdout}"
    );
    assert!(stdout.contains("\"agents\": []"), "empty roster: {stdout}");

    unsafe {
        libc::kill(daemon.id() as libc::pid_t, libc::SIGTERM);
    }
    let _ = daemon.wait();
    std::fs::remove_dir_all(home.root()).ok();
}

/// codex P1 on PR 924: the row-count assertion must hold on every read the
/// daemon serves, not only at startup. A daemon that starts on a healthy store
/// and then meets a NEWER writer's registry (one row this binary cannot
/// represent) must refuse the read, never serve the partial roster as the
/// complete one -- the startup assertion never re-runs.
#[tokio::test]
async fn registry_runtime_upgrade_refuses_a_partial_roster() {
    let _test_guard = daemon_e2e_guard();
    let home = short_home();
    home.ensure_root().unwrap();
    write_valid_registry(&home);
    let mut daemon = start_daemon(&home);

    // Barrier: the startup reconcile sweep's read-modify-write settles before
    // the future-schema fixture lands (same race the lookup test fences).
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let events = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
        if events.contains("daemon_started") {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    // A v15 store: the tolerant reader keeps the two rows it can represent and
    // drops the announced third (an unknown status value). Raw 3, decoded 2.
    let row = |name: &str, status: &str| {
        format!(
            r#"{{"name":"{name}","cwd":"/tmp/proj","harness":"claude","harness_session_id":"11111111-2222-3333-4444-555555555555","status":"{status}","created_at":"2026-08-16T00:00:00Z"}}"#
        )
    };
    let fixture = format!(
        r#"{{"schema_version":15,"agents":[{},{},{}]}}"#,
        row("worker-alpha", "live"),
        row("worker-beta", "flux"),
        row("worker-gamma", "live")
    );

    // The idle-tick sweeps (scrape, GC) read-modify-write the registry and can
    // HEAL a future-schema fixture mid-race (seen once on a slow CI runner):
    // the healed file is then a valid registry and serving it is correct. A
    // healed file breaks the test's precondition, not the code, so re-seed and
    // retry. A file still holding the 3-raw-row v15 shape that GETS SERVED is
    // the real failure and always panics here.
    let mut attempt = 0;
    loop {
        std::fs::write(home.registry_json(), &fixture).expect("seed future-schema registry");
        let out = Command::new(CLIENT_BIN)
            .args(["list", "--json"])
            .env("FNO_AGENTS_HOME", home.root())
            .output()
            .expect("client list runs");
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            assert!(
                stderr.contains("registry read failed"),
                "must name the failed read: {stderr}"
            );
            assert!(
                stderr.contains("raw_rows=3") && stderr.contains("decoded_rows=2"),
                "must carry both counts: {stderr}"
            );
            break;
        }
        attempt += 1;
        let on_disk = std::fs::read_to_string(home.registry_json()).unwrap_or_default();
        assert!(
            on_disk != fixture,
            "daemon served the 3-raw-row future-schema roster as complete: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        assert!(
            attempt < 5,
            "an idle-tick sweep keeps healing the fixture before the read; on-disk: {}",
            on_disk
        );
    }

    unsafe {
        libc::kill(daemon.id() as libc::pid_t, libc::SIGTERM);
    }
    let _ = daemon.wait();
    std::fs::remove_dir_all(home.root()).ok();
}
