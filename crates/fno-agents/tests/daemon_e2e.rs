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

/// AC1-HP (Architecture B, plan ab-70faa65b): a cold daemon start runs ONE
/// bounded reconcile sweep BEFORE serving, so the first `list` reads truthful
/// liveness. A stale `ask` row recorded `live` at creation (its one-shot process
/// long gone) settles to `exited` -- even though its provider session id makes it
/// "resumable" -- and surfaces that resumability via `session_id`. The startup
/// sweep is what flips it: no explicit `reconcile` RPC is issued here.
#[tokio::test]
async fn cold_start_reconciles_stale_ask_row_to_exited() {
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
        });
    })
    .unwrap();

    let mut daemon = start_daemon(&home);

    // The first served RPC necessarily follows the startup sweep (the accept loop
    // runs only after the sweep completes), so the listed status is post-sweep.
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

    // The startup sweep wrote the registry: status exited + CHECKED stamped.
    let reg = state::load_registry(&home.registry_json()).unwrap();
    let entry = reg.find("stale-ask").unwrap();
    assert_eq!(entry.status, fno_agents::AgentStatus::Exited);
    assert!(
        entry.last_reconciled_at.is_some(),
        "startup sweep must stamp last_reconciled_at (CHECKED freshens, AC1-UI)"
    );
    assert_eq!(entry.pid, None, "ask row never carries a pid");

    let events = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
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
        });
    })
    .unwrap();
}

// ---------------------------------------------------------------------------
// Daemon binary-version drift restart (ab-1891cdff): US2 (restart swaps the
// daemon), US3 (PTY workers survive -- Outcome B), US1/US4 (drift warned on
// list, stderr-only so --json stdout stays clean).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn restart_when_down_starts_fresh() {
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

    // AC4-HP: stdout is valid JSON with no warning text.
    serde_json::from_str::<serde_json::Value>(stdout.trim())
        .expect("list --json stdout is valid JSON");
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
