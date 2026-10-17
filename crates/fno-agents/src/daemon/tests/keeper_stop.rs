//! The keeper stop arm's cursor leg: a cursor-agent keeper-thread
//! row's detached worker-server is captured before the Kill and reaped after
//! the confirmation, and a reap that cannot end a captured server refuses
//! the terminal stamp. The non-cursor arm contracts (confirm-then-stamp,
//! refuse-on-silence) stay owned by keeper_sweep.rs.

use super::*;
use crate::daemon::tests::keeper_sweep::{keeper_sweep_home, spawn_killable_keeper};
use crate::daemon::tests::test_ctx;

fn read_events(home: &AgentsHome) -> Vec<Value> {
    let journal = home.events_jsonl();
    let _ = crate::event_store::import_all(&journal);
    crate::event_store::query_events(&journal, &crate::event_store::EventQuery::default())
        .unwrap_or_default()
        .iter()
        .filter_map(|r| serde_json::from_str::<Value>(&r.line).ok())
        .collect()
}

/// A cursor-agent lane-B row whose ownership anchor is THIS test process:
/// the census proves worker-server ownership through `pid` +
/// `pid_start_time`, so both must read back a live, real process.
fn cursor_thread_row(
    name: &str,
    session: &str,
    sock: &Path,
    worker_pid: Option<u32>,
) -> RegistryEntry {
    RegistryEntry {
        origin: Some("spawn".into()),
        name: name.into(),
        short_id: String::new(),
        legacy_provider: String::new(),
        harness: Some("cursor-agent".into()),
        harness_session_id: Some(session.into()),
        cwd: "/repo".into(),
        pid: Some(std::process::id()),
        pid_start_time: super::process_start_time(std::process::id()),
        keeper_child_pid: worker_pid,
        messaging_socket_path: Some(sock.to_string_lossy().into_owned()),
        host_mode: Some("interactive".into()),
        status: AgentStatus::Live,
        created_at: "2026-10-01T00:00:00Z".into(),
        ..Default::default()
    }
}

/// A real child whose ps command line names Cursor's worker-server: exec -a
/// keeps the marker on argv[0] across the exec, so the surviving image (a
/// plain `sleep`) still matches `is_cursor_worker_server_command`. `pre_exec`
/// runs BEFORE the exec (a trap set after it would never fire, and SIG_IGN
/// for TERM survives the exec into the sleep image). The start delay makes
/// the census deterministic: `ps` must list the child before stop's capture
/// runs. Returns the pid only: a reaper thread waits the child like a real
/// parent, because a reaped-then-dead pid would otherwise answer kill(0)
/// as a zombie and read as a survivor.
fn spawn_worker_server(pre_exec: &str) -> u32 {
    let mut child = std::process::Command::new("/bin/bash")
        .arg("-c")
        .arg(format!(
            "{pre_exec}; exec -a 'cursor-agent worker-server fake' /bin/sleep 30"
        ))
        .spawn()
        .expect("spawn worker-server fake");
    let pid = child.id();
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let listed = loop {
        let output = std::process::Command::new("ps")
            .args(["-axo", "command="])
            .output()
            .expect("ps");
        let seen = String::from_utf8_lossy(&output.stdout).contains("worker-server fake");
        if seen || std::time::Instant::now() > deadline {
            break seen;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    };
    assert!(listed, "the marker child never appeared in ps");
    pid
}

/// Serializes the two tests below around their fake worker-servers. Both
/// markers share one command string by design, so a census running while the
/// other test's TERM-immune child is alive would capture and reap it too and
/// read the survivor as a failure. One test process, one census at a time.
static WORKER_SERVER_CENSUS: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[tokio::test(flavor = "current_thread")]
async fn cursor_keeper_stop_reaps_the_worker_server_and_reports_the_count() {
    // AC1-HP: the keeper confirms, the captured worker-server pid is gone,
    // the row reads exited, and the receipt names the reap.
    let home = keeper_sweep_home("kpcap");
    let _census = WORKER_SERVER_CENSUS
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // The stop releases the stopped row's claims, which resolves the row-cwd
    // space dir; pin the state roots like keeper_sweep's stop test does.
    let _env = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    std::env::set_var("FNO_SPACES_DIR", home.root().join("spaces"));
    std::env::set_var("FNO_CLAIMS_ROOT", home.root().join("claims-root"));
    let sock = home
        .root()
        .join("mux")
        .join("threads")
        .join("cur-stop.sock");
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let keeper = spawn_killable_keeper(&sock, Arc::clone(&stop));
    let worker_pid = spawn_worker_server(":");
    state::update_registry(&home.registry_json(), |r| {
        r.entries.push(cursor_thread_row(
            "cur-stop",
            "csess-1",
            &sock,
            Some(worker_pid),
        ));
    })
    .unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("/nonexistent"));

    let response = handle_stop(
        &ctx,
        &Request::new(1, "agent.stop", json!({"name": "cur-stop"})),
    )
    .await;
    let result = match response.result() {
        Some(result) => result,
        None => panic!("stop errored: {:?}", response.error()),
    };
    assert_eq!(result["stopped"], true, "{result:?}");
    assert_eq!(result["backend"], "keeper-thread", "{result:?}");
    assert_eq!(result["worker_servers_reaped"], 1, "{result:?}");

    keeper.join().unwrap();
    // The reaped pid is really gone, not just reported gone.
    assert_ne!(
        unsafe { libc::kill(worker_pid as libc::pid_t, 0) },
        0,
        "the worker-server must be reaped, not reported"
    );
    let registry = load_registry_offloaded(home.registry_json()).await.unwrap();
    let entry = registry.find("cur-stop").unwrap();
    assert_eq!(entry.status, AgentStatus::Exited);
    assert!(entry.exited_at.is_some());
    assert!(read_events(&home).iter().any(|event| {
        event.get("type").and_then(Value::as_str) == Some("agent_stopped")
            && event
                .get("data")
                .and_then(|data| data["worker_servers_reaped"].as_i64())
                == Some(1)
    }));
    std::env::remove_var("FNO_SPACES_DIR");
    std::env::remove_var("FNO_CLAIMS_ROOT");
    std::fs::remove_dir_all(home.root()).ok();
}

#[tokio::test(flavor = "current_thread")]
async fn cursor_keeper_stop_refuses_the_stamp_when_a_server_survives_the_reap() {
    // AC1-ERR: a server that shrugs off SIGTERM (SIG_IGN crosses the exec)
    // fails the reap, the response is an error naming it, and the row stays
    // non-terminal so a retry can reach the leak.
    let home = keeper_sweep_home("kperr");
    let _census = WORKER_SERVER_CENSUS
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let sock = home
        .root()
        .join("mux")
        .join("threads")
        .join("cur-hard.sock");
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let keeper = spawn_killable_keeper(&sock, Arc::clone(&stop));
    let worker_pid = spawn_worker_server("trap \"\" TERM");
    state::update_registry(&home.registry_json(), |r| {
        r.entries.push(cursor_thread_row(
            "cur-hard",
            "csess-2",
            &sock,
            Some(worker_pid),
        ));
    })
    .unwrap();
    let ctx = test_ctx(home.clone(), PathBuf::from("/nonexistent"));

    let response = handle_stop(
        &ctx,
        &Request::new(2, "agent.stop", json!({"name": "cur-hard"})),
    )
    .await;
    assert!(
        response.result().is_none(),
        "a failed reap must error, not report a stop"
    );
    let message = response
        .error()
        .map(|error| error.message.clone())
        .unwrap_or_default();
    assert!(
        message.contains("worker-server reap failed"),
        "the error names the reap: {message}"
    );
    // The keeper DID confirm; only the reap failed, so the row must stay
    // live (a terminal stamp here would bury the surviving server).
    let registry = load_registry_offloaded(home.registry_json()).await.unwrap();
    assert_eq!(registry.find("cur-hard").unwrap().status, AgentStatus::Live);
    assert!(
        unsafe { libc::kill(worker_pid as libc::pid_t, 0) } == 0,
        "the stubborn server is the reason the reap failed"
    );

    // SAFETY: teardown of a test-spawned child that ignores SIGTERM. The
    // spawn-time reaper thread collects it.
    unsafe { libc::kill(worker_pid as libc::pid_t, libc::SIGKILL) };
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    keeper.join().unwrap();
    std::fs::remove_dir_all(home.root()).ok();
}
