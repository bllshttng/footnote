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
        "/tmp/abie{}_{}",
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

#[tokio::test]
async fn agent_survives_daemon_sigkill_and_daemon_reconnects_on_restart() {
    let home = short_home();
    home.ensure_root().unwrap();
    let daemon_bin = PathBuf::from(DAEMON_BIN);

    // The client lazy-start path forwards FNO_AGENTS_WORKER_BIN from our env.
    std::env::set_var("FNO_AGENTS_WORKER_BIN", WORKER_BIN);

    // --- Bring up the daemon (tracked so we have its PID) and spawn an agent
    //     whose PTY child is a long-lived `sleep`. ---
    let mut daemon = start_daemon(&home);
    let daemon_pid = daemon.id();

    let resp = call(
        &home,
        &daemon_bin,
        &Request::new(
            1,
            "agent.spawn",
            json!({"name": "survivor", "provider": "codex", "argv": ["sleep", "60"]}),
        ),
    )
    .await
    .expect("spawn call");
    assert!(!resp.is_err(), "spawn failed: {:?}", resp.error());
    let short_id = resp.result().unwrap()["short_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Worker PID from the registry; child PID from the worker's status RPC.
    let reg = state::load_registry(&home.registry_json()).unwrap();
    let worker_pid = reg
        .find("survivor")
        .unwrap()
        .pid
        .expect("worker pid recorded");
    let child_pid = worker_status_child_pid(&home, &short_id).await;
    assert!(pid_alive(worker_pid), "worker should be alive after spawn");
    assert!(
        pid_alive(child_pid),
        "sleep child should be alive after spawn"
    );

    // --- Kill the daemon hard. SIGKILL does not propagate to children, and the
    //     worker (not the daemon) owns the PTY master, so neither the worker nor
    //     its sleep child should die. ---
    unsafe {
        libc::kill(daemon_pid as libc::pid_t, libc::SIGKILL);
    }
    let _ = daemon.wait();
    std::thread::sleep(Duration::from_millis(1500));

    assert!(
        pid_alive(worker_pid),
        "OUTCOME B VIOLATED: worker died with the daemon"
    );
    assert!(
        pid_alive(child_pid),
        "OUTCOME B VIOLATED: PTY child died with the daemon"
    );

    // --- Restart the daemon. Recovery's worker-socket scan should rediscover the
    //     live worker and leave it alone; the agent stays reachable. ---
    let _ = std::fs::remove_file(home.supervisor_sock()); // stale socket from killed daemon
    let mut daemon2 = start_daemon(&home);

    let listed = call(
        &home,
        &daemon_bin,
        &Request::new(2, "agent.list", json!({"all": true})),
    )
    .await
    .expect("list call");
    let agents = listed.result().unwrap()["agents"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        agents.iter().any(|a| a["name"] == "survivor"),
        "restarted daemon lost the survivor agent"
    );

    // The worker socket is still the same live worker (PID unchanged).
    assert!(
        pid_alive(worker_pid),
        "worker should still be the same process"
    );

    // --- Cleanup: stop the agent (worker shuts its child down) and the daemon. ---
    let _ = call(
        &home,
        &daemon_bin,
        &Request::new(3, "agent.stop", json!({"name": "survivor"})),
    )
    .await;
    std::thread::sleep(Duration::from_millis(300));
    unsafe {
        libc::kill(daemon2.id() as libc::pid_t, libc::SIGTERM);
    }
    let _ = daemon2.wait();
    // Belt-and-suspenders: ensure no strays.
    unsafe {
        libc::kill(worker_pid as libc::pid_t, libc::SIGKILL);
        libc::kill(child_pid as libc::pid_t, libc::SIGKILL);
    }
    std::fs::remove_dir_all(home.root()).ok();
}

async fn worker_status_child_pid(home: &AgentsHome, short_id: &str) -> u32 {
    use fno_agents::protocol::{read_response, write_request};
    use tokio::net::UnixStream;
    let sock = home.worker_sock(short_id);
    let mut conn = UnixStream::connect(&sock)
        .await
        .expect("connect worker.sock");
    write_request(&mut conn, &Request::new(99, "worker.status", json!({})))
        .await
        .unwrap();
    let resp = read_response(&mut conn).await.unwrap();
    resp.result().unwrap()["child_pid"].as_u64().unwrap() as u32
}

#[tokio::test]
async fn spawn_rejects_name_collision_and_lists_per_project() {
    let home = short_home();
    home.ensure_root().unwrap();
    let daemon_bin = PathBuf::from(DAEMON_BIN);
    std::env::set_var("FNO_AGENTS_WORKER_BIN", WORKER_BIN);
    let mut daemon = start_daemon(&home);

    let mk = |id: u64, name: &str| {
        Request::new(
            id,
            "agent.spawn",
            json!({"name": name, "provider": "codex", "cwd": "/tmp/projX", "argv": ["sleep", "30"]}),
        )
    };

    let a = call(&home, &daemon_bin, &mk(1, "dup")).await.unwrap();
    assert!(!a.is_err(), "first spawn ok: {:?}", a.error());

    let b = call(&home, &daemon_bin, &mk(2, "dup")).await.unwrap();
    assert!(b.is_err(), "second spawn must collide");

    // list filtered by a non-matching project_root shows nothing.
    let none = call(
        &home,
        &daemon_bin,
        &Request::new(3, "agent.list", json!({"project_root": "/tmp/other"})),
    )
    .await
    .unwrap();
    assert_eq!(
        none.result().unwrap()["agents"].as_array().unwrap().len(),
        0
    );

    // list filtered by the right project_root shows the agent.
    let one = call(
        &home,
        &daemon_bin,
        &Request::new(4, "agent.list", json!({"project_root": "/tmp/projX"})),
    )
    .await
    .unwrap();
    assert_eq!(one.result().unwrap()["agents"].as_array().unwrap().len(), 1);

    let _ = call(
        &home,
        &daemon_bin,
        &Request::new(5, "agent.stop", json!({"name": "dup"})),
    )
    .await;
    std::thread::sleep(Duration::from_millis(200));
    unsafe {
        libc::kill(daemon.id() as libc::pid_t, libc::SIGTERM);
    }
    let _ = daemon.wait();
    std::fs::remove_dir_all(home.root()).ok();
}

#[tokio::test]
async fn channel_register_unregister_push_roundtrip() {
    let home = short_home();
    home.ensure_root().unwrap();
    let daemon_bin = PathBuf::from(DAEMON_BIN);
    std::env::set_var("FNO_AGENTS_WORKER_BIN", WORKER_BIN);
    let mut daemon = start_daemon(&home);

    // Spawn an agent to attach a channel to.
    call(
        &home,
        &daemon_bin,
        &Request::new(
            1,
            "agent.spawn",
            json!({"name": "chan", "provider": "codex", "argv": ["sleep", "30"]}),
        ),
    )
    .await
    .unwrap();

    // register_channel against an unknown cc_session_id -> channel_unknown.
    // (register requires a matching agent by name OR cc_session_id; we pass name.)
    let reg = call(
        &home,
        &daemon_bin,
        &Request::new(
            2,
            "channel.register_channel",
            json!({"cc_session_id": "cc-123", "name": "chan"}),
        ),
    )
    .await
    .unwrap();
    assert!(!reg.is_err(), "register failed: {:?}", reg.error());
    let channel_id = reg.result().unwrap()["mcp_channel_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(channel_id.len(), 36);

    // push to the registered channel -> routed.
    let push = call(
        &home,
        &daemon_bin,
        &Request::new(
            3,
            "channel.push_to_channel",
            json!({"mcp_channel_id": channel_id}),
        ),
    )
    .await
    .unwrap();
    assert!(!push.is_err(), "push failed: {:?}", push.error());

    // push to an unknown channel -> channel_unknown.
    let bad = call(
        &home,
        &daemon_bin,
        &Request::new(
            4,
            "channel.push_to_channel",
            json!({"mcp_channel_id": "deadbeef"}),
        ),
    )
    .await
    .unwrap();
    assert!(bad.is_err());

    let _ = call(
        &home,
        &daemon_bin,
        &Request::new(5, "agent.stop", json!({"name": "chan"})),
    )
    .await;
    std::thread::sleep(Duration::from_millis(200));
    unsafe {
        libc::kill(daemon.id() as libc::pid_t, libc::SIGTERM);
    }
    let _ = daemon.wait();
    std::fs::remove_dir_all(home.root()).ok();
}

/// Regression for the sigma-review fixes: an agent whose PTY child exits on its
/// own must (a) have been spawned successfully (fix B: spawn confirmed a live
/// child), (b) emit an `agent_exited` event to events.jsonl when the child dies
/// (fix D #2: steady-state death is observable), and (c) flip its registry row
/// to `exited` without waiting for a daemon restart (fix D #1).
#[tokio::test]
async fn agent_exit_is_observable_in_events_and_registry() {
    let home = short_home();
    home.ensure_root().unwrap();
    let daemon_bin = PathBuf::from(DAEMON_BIN);
    std::env::set_var("FNO_AGENTS_WORKER_BIN", WORKER_BIN);
    let mut daemon = start_daemon(&home);

    // The child must still be alive when the daemon performs its spawn-confirm
    // liveness check, then exit on its own so the worker emits agent_exited.
    // `sleep 4` (not 1) keeps the spawn-confirm window robust under heavy
    // parallel-test load: with the full suite running, worker startup + the
    // confirm round-trip can take well over a second, and a `sleep 1` child can
    // exit first, spuriously failing spawn with "PTY child is not alive".
    let resp = call(
        &home,
        &daemon_bin,
        &Request::new(
            1,
            "agent.spawn",
            json!({"name": "ephemeral", "provider": "codex", "argv": ["sleep", "4"]}),
        ),
    )
    .await
    .expect("spawn call");
    assert!(
        !resp.is_err(),
        "spawn should succeed for a live child: {:?}",
        resp.error()
    );
    let short_id = resp.result().unwrap()["short_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Wait past the child's exit (`sleep 4`) + the worker's 250ms liveness tick,
    // with margin for parallel-test load.
    std::thread::sleep(Duration::from_millis(6000));

    // (b) events.jsonl carries an agent_exited line for this agent (keyed by the
    // daemon-assigned short_id, which is derived/truncated from the name).
    let events = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
    let saw_exit = events
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .any(|e| e["kind"] == "agent_exited" && e["short_id"] == short_id);
    assert!(saw_exit, "expected an agent_exited event in events.jsonl");

    // (c) the registry shows the agent exited without a daemon restart.
    let reg = state::load_registry(&home.registry_json()).unwrap();
    let status = reg.find("ephemeral").map(|e| e.status);
    assert_eq!(
        status,
        Some(fno_agents::AgentStatus::Exited),
        "registry should show exited"
    );

    unsafe {
        libc::kill(daemon.id() as libc::pid_t, libc::SIGTERM);
    }
    let _ = daemon.wait();
    std::fs::remove_dir_all(home.root()).ok();
}

/// reconcile (Wave 5, US6.9): a live agent (its worker pid alive) is preserved
/// via the fast path, gets `last_reconciled_at` stamped, and the response
/// carries the locked shape (updated/orphans/recovered/deferred).
#[tokio::test]
async fn reconcile_preserves_live_agent_and_stamps_last_reconciled() {
    let home = short_home();
    home.ensure_root().unwrap();
    let daemon_bin = PathBuf::from(DAEMON_BIN);
    std::env::set_var("FNO_AGENTS_WORKER_BIN", WORKER_BIN);
    let mut daemon = start_daemon(&home);

    let resp = call(
        &home,
        &daemon_bin,
        &Request::new(
            1,
            "agent.spawn",
            json!({"name": "recon", "provider": "codex", "argv": ["sleep", "30"]}),
        ),
    )
    .await
    .expect("spawn call");
    assert!(!resp.is_err(), "spawn failed: {:?}", resp.error());

    let reconciled = call(
        &home,
        &daemon_bin,
        &Request::new(2, "agent.reconcile", json!({})),
    )
    .await
    .expect("reconcile call");
    assert!(
        !reconciled.is_err(),
        "reconcile failed: {:?}",
        reconciled.error()
    );
    let r = reconciled.result().unwrap();
    // Legacy internal keys (backward compat).
    for key in [
        "updated",
        "orphans",
        "recovered",
        "inconsistent",
        "deferred",
    ] {
        assert!(r.get(key).is_some(), "reconcile response missing `{key}`");
    }
    // Python-parity keys (Task 3.1 contract). Previously the test asserted only
    // the legacy keys and would not catch a regression in the parity shape
    // (cv-789fdba0 part c).
    for key in ["scanned", "orphaned", "recovered", "skipped", "errors"] {
        assert!(
            r.get(key).is_some(),
            "reconcile response missing parity key `{key}`: {r}"
        );
    }
    // One probed entry, none deferred: scanned counts all entries (matches
    // Python `scanned=len(entries)`, cv-5b1a4164) and skipped is empty.
    assert_eq!(
        r["scanned"].as_u64().unwrap(),
        1,
        "scanned must count entries"
    );
    assert_eq!(
        r["skipped"].as_array().unwrap().len(),
        0,
        "no deferred entries -> skipped empty"
    );
    // A live agent (worker pid alive) is not orphaned by reconcile.
    assert_eq!(
        r["orphans"].as_array().unwrap().len(),
        0,
        "a live agent must not be orphaned"
    );

    let reg = state::load_registry(&home.registry_json()).unwrap();
    let entry = reg.find("recon").unwrap();
    assert_eq!(entry.status, fno_agents::AgentStatus::Live);
    assert!(
        entry.last_reconciled_at.is_some(),
        "reconcile must stamp last_reconciled_at on a probed entry"
    );
    let events = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
    assert!(
        events.contains("reconcile_done"),
        "reconcile_done event not emitted"
    );

    let _ = call(
        &home,
        &daemon_bin,
        &Request::new(3, "agent.stop", json!({"name": "recon"})),
    )
    .await;
    std::thread::sleep(Duration::from_millis(200));
    unsafe {
        libc::kill(daemon.id() as libc::pid_t, libc::SIGTERM);
    }
    let _ = daemon.wait();
    std::fs::remove_dir_all(home.root()).ok();
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
            provider: "codex".into(),
            cwd: "/tmp".into(),
            project_root: String::new(),
            session_id: None,
            claude_short_id: None,
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
        row["status"], "exited",
        "a finished ask must settle to exited on cold-start reconcile"
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
    // Last-recorded status preserved: a failed sweep applies no change.
    assert_eq!(row["status"], "live", "failed sweep must not mutate status");

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

/// status (Wave 5, US6.10): when the daemon is up, `agent.status` returns the
/// locked status-v1.json shape with all required top-level sections.
#[tokio::test]
async fn status_reports_v1_shape_when_daemon_up() {
    let home = short_home();
    home.ensure_root().unwrap();
    let daemon_bin = PathBuf::from(DAEMON_BIN);
    std::env::set_var("FNO_AGENTS_WORKER_BIN", WORKER_BIN);
    let mut daemon = start_daemon(&home);

    call(
        &home,
        &daemon_bin,
        &Request::new(
            1,
            "agent.spawn",
            json!({"name": "statme", "provider": "codex", "argv": ["sleep", "30"]}),
        ),
    )
    .await
    .expect("spawn call");

    let status = call(
        &home,
        &daemon_bin,
        &Request::new(2, "agent.status", json!({})),
    )
    .await
    .expect("status call");
    assert!(!status.is_err(), "status failed: {:?}", status.error());
    let s = status.result().unwrap();
    assert_eq!(s["schema_version"], 1);
    assert_eq!(s["daemon"]["state"], "serving");
    assert!(s["daemon"]["pid"].as_u64().is_some());
    assert!(s["daemon"]["uptime_secs"].as_u64().is_some());
    assert_eq!(s["agents"]["total"], 1);
    assert_eq!(s["agents"]["by_status"]["live"], 1);
    assert_eq!(s["drives"]["active"], 0);
    assert!(s["restarts"]["queue_depth"].as_u64().is_some());
    assert!(s["restarts"]["consecutive_failures_max_seen"]
        .as_u64()
        .is_some());
    assert_eq!(s["channels"]["registered"], 0);

    let _ = call(
        &home,
        &daemon_bin,
        &Request::new(3, "agent.stop", json!({"name": "statme"})),
    )
    .await;
    std::thread::sleep(Duration::from_millis(200));
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

/// list (Wave 5, US6.6): the per-agent row carries last_message_at (present as
/// a key even when null), alongside the existing name/provider/status fields.
#[tokio::test]
async fn list_row_includes_last_message_at_field() {
    let home = short_home();
    home.ensure_root().unwrap();
    let daemon_bin = PathBuf::from(DAEMON_BIN);
    std::env::set_var("FNO_AGENTS_WORKER_BIN", WORKER_BIN);
    let mut daemon = start_daemon(&home);

    call(
        &home,
        &daemon_bin,
        &Request::new(
            1,
            "agent.spawn",
            json!({"name": "listme", "provider": "codex", "argv": ["sleep", "30"]}),
        ),
    )
    .await
    .expect("spawn call");

    let listed = call(
        &home,
        &daemon_bin,
        &Request::new(2, "agent.list", json!({"all": true})),
    )
    .await
    .expect("list call");
    let row = listed.result().unwrap()["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "listme")
        .expect("listme present")
        .clone();
    // Full projection shape: the 10 parity keys + the daemon's project_root
    // superset + the additive Architecture C keys pid + last_reconciled_at (plan
    // ab-70faa65b). Asserting the full set catches a regression dropping any
    // field (cv-789fdba0 part b).
    for key in [
        "name",
        "provider",
        "short_id",
        "session_id",
        "cwd",
        "created_at",
        "last_message_at",
        "status",
        "live_status",
        "log_path",
        "project_root",
        "pid",
        "last_reconciled_at",
    ] {
        assert!(row.get(key).is_some(), "list row missing `{key}`: {row}");
    }
    // live_status is intentionally null and RETAINED for back-compat (cv-eeaad75d
    // / Locked #4): the daemon does not replicate Python's `claude agents --json`
    // augmentation; PTY-worker status is the canonical signal.
    assert!(
        row["live_status"].is_null(),
        "live_status should be null (augmentation not replicated): {row}"
    );
    // AC4-HP: a live PTY worker exposes its worker pid in --json.
    assert!(
        row["pid"].as_u64().is_some(),
        "a PTY worker row must carry a numeric pid: {row}"
    );

    let _ = call(
        &home,
        &daemon_bin,
        &Request::new(3, "agent.stop", json!({"name": "listme"})),
    )
    .await;
    std::thread::sleep(Duration::from_millis(200));
    unsafe {
        libc::kill(daemon.id() as libc::pid_t, libc::SIGTERM);
    }
    let _ = daemon.wait();
    std::fs::remove_dir_all(home.root()).ok();
}

#[tokio::test]
async fn interactive_spawn_persists_host_mode_and_reaches_live() {
    // Task 2.2 + 2.1 integration (interactive-drive node ab-26b5fe82): a spawn
    // with host_mode=interactive whose PTY child paints then stays alive passes
    // the interactive readiness gate (paint + survives the dwell) and is
    // registered host_mode=interactive, status=live. A real `codex`/`gemini`
    // TUI needs auth, so the child here is a paints-then-sleeps stand-in injected
    // via --argv (the gate is provider-agnostic).
    let home = short_home();
    home.ensure_root().unwrap();
    let daemon_bin = PathBuf::from(DAEMON_BIN);
    std::env::set_var("FNO_AGENTS_WORKER_BIN", WORKER_BIN);
    let mut daemon = start_daemon(&home);

    let resp = call(
        &home,
        &daemon_bin,
        &Request::new(
            1,
            "agent.spawn",
            json!({
                "name": "hosttui",
                "provider": "codex",
                "host_mode": "interactive",
                "argv": ["bash", "-c", "printf 'TUI READY\\n'; sleep 30"],
            }),
        ),
    )
    .await
    .expect("spawn call");
    assert!(
        !resp.is_err(),
        "interactive spawn failed: {:?}",
        resp.error()
    );

    let reg = state::load_registry(&home.registry_json()).unwrap();
    let row = reg.find("hosttui").expect("hosttui registered");
    assert_eq!(
        row.host_mode_or_default(),
        "interactive",
        "row must persist host_mode=interactive"
    );
    assert_eq!(row.status, fno_agents::AgentStatus::Live);

    let _ = call(
        &home,
        &daemon_bin,
        &Request::new(2, "agent.stop", json!({"name": "hosttui"})),
    )
    .await;
    std::thread::sleep(Duration::from_millis(200));
    unsafe {
        libc::kill(daemon.id() as libc::pid_t, libc::SIGTERM);
    }
    let _ = daemon.wait();
    std::fs::remove_dir_all(home.root()).ok();
}

#[tokio::test]
async fn interactive_spawn_fails_when_child_dies_before_ready() {
    // AC1-FR / AC1-UI: an interactive spawn whose child exits immediately (the
    // stand-in for a `codex resume`/`gemini -r` that fails at launch) must report
    // spawn-failed -- never a live row that instantly exits -- and leave no live
    // registry entry.
    let home = short_home();
    home.ensure_root().unwrap();
    let daemon_bin = PathBuf::from(DAEMON_BIN);
    std::env::set_var("FNO_AGENTS_WORKER_BIN", WORKER_BIN);
    let mut daemon = start_daemon(&home);

    let resp = call(
        &home,
        &daemon_bin,
        &Request::new(
            1,
            "agent.spawn",
            json!({
                "name": "deadtui",
                "provider": "codex",
                "host_mode": "interactive",
                "argv": ["bash", "-c", "printf 'boom: session not found\\n'; exit 1"],
            }),
        ),
    )
    .await
    .expect("spawn call");
    assert!(
        resp.is_err(),
        "a child that dies before readiness must be spawn-failed, got: {:?}",
        resp.result()
    );

    // No live row should survive the failed spawn.
    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert!(
        reg.find("deadtui").is_none(),
        "a failed interactive spawn must not leave a registry row"
    );

    unsafe {
        libc::kill(daemon.id() as libc::pid_t, libc::SIGTERM);
    }
    let _ = daemon.wait();
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
            provider: "codex".into(),
            cwd: "/tmp".into(),
            project_root: String::new(),
            session_id: None,
            claude_short_id: None,
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
        });
    })
    .unwrap();
}

#[tokio::test]
async fn promote_happy_path_persists_interactive_with_session_id() {
    // AC1-HP end-to-end: a settled exec source (codex_session_id=U, exited) is
    // promoted via the agent.spawn IPC with host_mode=interactive + resume_id=U.
    // The new row infers provider=codex, carries codex_session_id=U +
    // host_mode=interactive, and reaches live. A real `codex resume` needs auth,
    // so --argv injects a paints-then-sleeps stand-in (admit_promote runs off
    // resume_id, independent of argv).
    let home = short_home();
    home.ensure_root().unwrap();
    let daemon_bin = PathBuf::from(DAEMON_BIN);
    std::env::set_var("FNO_AGENTS_WORKER_BIN", WORKER_BIN);
    let uuid = "019e7157-4236-7bb1-b274-ebbac6040ace";
    seed_codex_source(&home, "src", uuid, fno_agents::AgentStatus::Exited);
    let mut daemon = start_daemon(&home);

    let resp = call(
        &home,
        &daemon_bin,
        &Request::new(
            1,
            "agent.spawn",
            json!({
                "name": "bot2",
                "host_mode": "interactive",
                "resume_id": uuid,
                "argv": ["bash", "-c", "printf 'RESUMED\\n'; sleep 30"],
            }),
        ),
    )
    .await
    .expect("spawn call");
    assert!(!resp.is_err(), "promote failed: {:?}", resp.error());

    let reg = state::load_registry(&home.registry_json()).unwrap();
    let row = reg.find("bot2").expect("bot2 registered");
    assert!(row.is_interactive(), "bot2 must be host_mode=interactive");
    assert_eq!(row.provider, "codex", "provider inferred from source row");
    assert_eq!(
        row.codex_session_id.as_deref(),
        Some(uuid),
        "promoted row must record the resumed session id"
    );
    assert_eq!(row.status, fno_agents::AgentStatus::Live);

    let _ = call(
        &home,
        &daemon_bin,
        &Request::new(2, "agent.stop", json!({"name": "bot2"})),
    )
    .await;
    std::thread::sleep(Duration::from_millis(200));
    unsafe {
        libc::kill(daemon.id() as libc::pid_t, libc::SIGTERM);
    }
    let _ = daemon.wait();
    std::fs::remove_dir_all(home.root()).ok();
}

#[tokio::test]
async fn promote_unknown_uuid_rejected_e2e() {
    // AC1-ERR: promote --from <unknown> is rejected at the daemon (no row left).
    let home = short_home();
    home.ensure_root().unwrap();
    let daemon_bin = PathBuf::from(DAEMON_BIN);
    std::env::set_var("FNO_AGENTS_WORKER_BIN", WORKER_BIN);
    let mut daemon = start_daemon(&home);

    let resp = call(
        &home,
        &daemon_bin,
        &Request::new(
            1,
            "agent.spawn",
            json!({"name": "ghost", "host_mode": "interactive", "resume_id": "no-such-uuid"}),
        ),
    )
    .await
    .expect("spawn call");
    assert!(resp.is_err(), "unknown uuid must be rejected");
    assert!(
        resp.error().unwrap().message.contains("unknown session"),
        "message should name the unknown session: {:?}",
        resp.error()
    );
    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert!(reg.find("ghost").is_none(), "no row for a rejected promote");

    unsafe {
        libc::kill(daemon.id() as libc::pid_t, libc::SIGTERM);
    }
    let _ = daemon.wait();
    std::fs::remove_dir_all(home.root()).ok();
}

#[tokio::test]
async fn promote_still_running_source_rejected_e2e() {
    // AC2-ERR: a source in a mid-flight state (Busy) is not promotable.
    let home = short_home();
    home.ensure_root().unwrap();
    let daemon_bin = PathBuf::from(DAEMON_BIN);
    std::env::set_var("FNO_AGENTS_WORKER_BIN", WORKER_BIN);
    let uuid = "11111111-2222-3333-4444-555555555555";
    seed_codex_source(&home, "busysrc", uuid, fno_agents::AgentStatus::Busy);
    // Opt out of the startup reconcile sweep: the seeded source is ask-shaped
    // (empty short_id), so the sweep would settle its mid-flight `Busy` status to
    // `exited` before promote admission runs, defeating the "still running"
    // rejection this test asserts. The sweep is verified separately
    // (cold_start_reconciles_stale_ask_row_to_exited).
    let mut daemon = start_daemon_env(&home, &[("FNO_AGENTS_NO_STARTUP_RECONCILE", "1")]);

    let resp = call(
        &home,
        &daemon_bin,
        &Request::new(
            1,
            "agent.spawn",
            json!({"name": "bot2", "host_mode": "interactive", "resume_id": uuid}),
        ),
    )
    .await
    .expect("spawn call");
    assert!(resp.is_err(), "still-running source must be rejected");
    assert!(
        resp.error().unwrap().message.contains("still running"),
        "message should say still running: {:?}",
        resp.error()
    );

    unsafe {
        libc::kill(daemon.id() as libc::pid_t, libc::SIGTERM);
    }
    let _ = daemon.wait();
    std::fs::remove_dir_all(home.root()).ok();
}

#[tokio::test]
async fn host_bad_provider_rejected_e2e() {
    // AC3-ERR: a fresh interactive host with an unsupported provider is rejected
    // before any worker spawns. claude is now a supported interactive provider
    // (the stream-json adopt lane, ab-734fcd6c), so this uses a genuinely
    // unsupported CLI to exercise the PTY-provider gate.
    let home = short_home();
    home.ensure_root().unwrap();
    let daemon_bin = PathBuf::from(DAEMON_BIN);
    std::env::set_var("FNO_AGENTS_WORKER_BIN", WORKER_BIN);
    let mut daemon = start_daemon(&home);

    let resp = call(
        &home,
        &daemon_bin,
        &Request::new(
            1,
            "agent.spawn",
            json!({"name": "badtui", "provider": "opencode", "host_mode": "interactive", "message": "x"}),
        ),
    )
    .await
    .expect("spawn call");
    assert!(
        resp.is_err(),
        "unsupported interactive provider must be rejected"
    );
    assert!(
        resp.error()
            .unwrap()
            .message
            .contains("only codex, gemini, agy, or claude"),
        "message should explain the provider constraint: {:?}",
        resp.error()
    );
    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert!(reg.find("badtui").is_none());

    unsafe {
        libc::kill(daemon.id() as libc::pid_t, libc::SIGTERM);
    }
    let _ = daemon.wait();
    std::fs::remove_dir_all(home.root()).ok();
}

#[tokio::test]
async fn promote_already_hosted_rejected_e2e() {
    // AC2-EDGE: once a session is hosted by a live interactive agent, a second
    // promote of the same UUID is rejected (one-host invariant), end-to-end.
    let home = short_home();
    home.ensure_root().unwrap();
    let daemon_bin = PathBuf::from(DAEMON_BIN);
    std::env::set_var("FNO_AGENTS_WORKER_BIN", WORKER_BIN);
    let uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    seed_codex_source(&home, "src", uuid, fno_agents::AgentStatus::Exited);
    let mut daemon = start_daemon(&home);

    // First promote -> a live interactive host on the session.
    let first = call(
        &home,
        &daemon_bin,
        &Request::new(
            1,
            "agent.spawn",
            json!({
                "name": "host1",
                "host_mode": "interactive",
                "resume_id": uuid,
                "argv": ["bash", "-c", "printf 'UP\\n'; sleep 30"],
            }),
        ),
    )
    .await
    .expect("first promote call");
    assert!(
        !first.is_err(),
        "first promote should succeed: {:?}",
        first.error()
    );

    // Second promote of the same UUID -> rejected by the one-host invariant.
    let second = call(
        &home,
        &daemon_bin,
        &Request::new(
            2,
            "agent.spawn",
            json!({"name": "host2", "host_mode": "interactive", "resume_id": uuid}),
        ),
    )
    .await
    .expect("second promote call");
    assert!(
        second.is_err(),
        "second promote of a hosted session must be rejected"
    );
    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert!(
        reg.find("host2").is_none(),
        "rejected promote leaves no row"
    );

    let _ = call(
        &home,
        &daemon_bin,
        &Request::new(3, "agent.stop", json!({"name": "host1"})),
    )
    .await;
    std::thread::sleep(Duration::from_millis(200));
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
async fn restart_swaps_daemon_and_worker_survives() {
    // AC2-HP + AC3-HP: `restart` SIGTERMs the old daemon and lazy-starts a fresh
    // one (OLD != NEW), while a live PTY worker survives and is re-adopted.
    let home = short_home();
    home.ensure_root().unwrap();
    let daemon_bin = PathBuf::from(DAEMON_BIN);
    std::env::set_var("FNO_AGENTS_WORKER_BIN", WORKER_BIN);

    let mut daemon = start_daemon(&home);
    let old_pid = daemon.id();

    // A long-lived PTY worker whose child is `sleep`.
    let resp = call(
        &home,
        &daemon_bin,
        &Request::new(
            1,
            "agent.spawn",
            json!({"name": "survivor", "provider": "codex", "argv": ["sleep", "60"]}),
        ),
    )
    .await
    .expect("spawn call");
    assert!(!resp.is_err(), "spawn failed: {:?}", resp.error());
    let short_id = resp.result().unwrap()["short_id"]
        .as_str()
        .unwrap()
        .to_string();
    let worker_pid = state::load_registry(&home.registry_json())
        .unwrap()
        .find("survivor")
        .unwrap()
        .pid
        .expect("worker pid");
    let child_pid = worker_status_child_pid(&home, &short_id).await;
    assert!(pid_alive(worker_pid) && pid_alive(child_pid));

    // Restart: SIGTERM old, fresh new.
    let outcome = fno_agents::client::restart_daemon(&home, &daemon_bin)
        .await
        .expect("restart succeeds");
    let _ = daemon.wait(); // reap the SIGTERM'd tracked child
    assert_eq!(
        outcome.old_pid,
        Some(old_pid),
        "restart reports the old pid"
    );
    assert_ne!(outcome.new_pid, old_pid, "fresh daemon has a new pid");
    assert!(pid_alive(outcome.new_pid), "fresh daemon is alive");

    // Outcome B: the worker and its child outlived the SIGTERM.
    assert!(
        pid_alive(worker_pid),
        "OUTCOME B VIOLATED: worker died on restart"
    );
    assert!(
        pid_alive(child_pid),
        "OUTCOME B VIOLATED: PTY child died on restart"
    );

    // The fresh daemon re-adopted it (list shows it).
    let listed = call(
        &home,
        &daemon_bin,
        &Request::new(2, "agent.list", json!({"all": true})),
    )
    .await
    .expect("list call");
    assert!(
        listed.result().unwrap()["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["name"] == "survivor"),
        "restarted daemon lost the survivor agent"
    );

    // Cleanup.
    let _ = call(
        &home,
        &daemon_bin,
        &Request::new(3, "agent.stop", json!({"name": "survivor"})),
    )
    .await;
    std::thread::sleep(Duration::from_millis(300));
    unsafe {
        libc::kill(outcome.new_pid as libc::pid_t, libc::SIGTERM);
        libc::kill(worker_pid as libc::pid_t, libc::SIGKILL);
        libc::kill(child_pid as libc::pid_t, libc::SIGKILL);
    }
    std::fs::remove_dir_all(home.root()).ok();
}

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
        "/tmp/abidcopy_{}_{}",
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
    // copy's fingerprint at startup.
    let mut daemon = Command::new(&dcopy)
        .env("FNO_AGENTS_HOME", home.root())
        .env("FNO_AGENTS_WORKER_BIN", WORKER_BIN)
        .env("FNO_AGENTS_IDLE_EXIT_SECS", "3600")
        .spawn()
        .expect("daemon spawns from copy");
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
